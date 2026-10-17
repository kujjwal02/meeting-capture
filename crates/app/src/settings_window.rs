//! The settings window: plain HTML in `ui/`, calling the commands here. It edits the settings
//! file and the keychain. Google account changes apply at once; the rest after a restart.

use std::path::PathBuf;
use std::sync::{MutexGuard, PoisonError};

use pipeline::calendar::AccountStatus;
use pipeline::google::{self, Keychain, OAuthClient};
use pipeline::settings::SettingsFile;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt as _;

use crate::AppState;

const WINDOW_LABEL: &str = "settings";

/// Shows the settings window, or brings it forward when it is already open.
pub(crate) fn open(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW_LABEL) {
        if let Err(error) = window.set_focus() {
            tracing::warn!(exception.message = %error, "could not focus the settings window");
        }
        return;
    }
    let built = WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
        .title("Meeting Capture settings")
        .inner_size(560.0, 720.0)
        .focused(true)
        .build();
    if let Err(error) = built {
        tracing::error!(exception.message = %error, "could not open the settings window");
    }
}

#[derive(Serialize)]
pub(crate) struct SettingsView {
    vault_dir: Option<PathBuf>,
    my_name: Option<String>,
    receiver_port: Option<u16>,
    time_zone: Option<String>,
    client_id: Option<String>,
    has_client_secret: bool,
    accounts: Vec<AccountView>,
    settings_file: PathBuf,
}

#[derive(Serialize)]
pub(crate) struct AccountView {
    email: String,
    needs_sign_in: bool,
}

/// Blank fields mean "use the default". A blank client secret keeps the stored one.
#[derive(Deserialize)]
pub(crate) struct SettingsForm {
    vault_dir: String,
    my_name: String,
    receiver_port: Option<u16>,
    time_zone: String,
    client_id: String,
    client_secret: String,
}

#[derive(Serialize)]
pub(crate) struct ClientFields {
    client_id: String,
    client_secret: String,
}

fn keychain(state: &AppState) -> Keychain {
    Keychain::new(state.app_dirs.keychain_service())
}

fn account_statuses(state: &AppState, file: &SettingsFile) -> Vec<AccountStatus> {
    match &state.pipeline {
        Some(pipeline) => pipeline.google().statuses(),
        // Without a running pipeline nothing has checked the accounts.
        None => file
            .google
            .accounts
            .iter()
            .map(|email| AccountStatus {
                email: email.clone(),
                needs_sign_in: false,
            })
            .collect(),
    }
}

// The window and the tray read the new account list at once; other fields wait for a restart.
fn apply_google(app: &AppHandle, file: &SettingsFile) {
    if let Some(pipeline) = &app.state::<AppState>().pipeline {
        pipeline.google().replace(&file.google);
    }
    crate::tray::refresh(app);
}

fn lock_settings_file(state: &AppState) -> MutexGuard<'_, ()> {
    state
        .settings_file_lock
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn non_blank(text: &str) -> Option<String> {
    Some(text.trim().to_owned()).filter(|text| !text.is_empty())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri passes command arguments by value"
)]
pub(crate) fn settings_load(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let file = SettingsFile::read(&state.app_dirs).map_err(|error| error.to_string())?;
    let has_client_secret = keychain(&state)
        .has_client_secret()
        .map_err(|error| error.to_string())?;
    let accounts = account_statuses(&state, &file)
        .into_iter()
        .map(|account| AccountView {
            email: account.email,
            needs_sign_in: account.needs_sign_in,
        })
        .collect();
    Ok(SettingsView {
        vault_dir: file.vault_dir,
        my_name: file.my_name,
        receiver_port: file.receiver_port,
        time_zone: file.time_zone,
        client_id: file.google.client_id,
        has_client_secret,
        accounts,
        settings_file: state.app_dirs.settings_file(),
    })
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri passes command arguments by value"
)]
pub(crate) fn settings_save(
    app: AppHandle,
    state: State<'_, AppState>,
    form: SettingsForm,
) -> Result<(), String> {
    let _settings_file = lock_settings_file(&state);
    let mut file = SettingsFile::read(&state.app_dirs).map_err(|error| error.to_string())?;
    file.vault_dir = non_blank(&form.vault_dir).map(PathBuf::from);
    file.my_name = non_blank(&form.my_name);
    file.receiver_port = form.receiver_port;
    file.time_zone = non_blank(&form.time_zone);
    file.google.client_id = non_blank(&form.client_id);
    file.write(&state.app_dirs)
        .map_err(|error| error.to_string())?;
    let has_new_secret = match non_blank(&form.client_secret) {
        Some(client_secret) => {
            keychain(&state)
                .set_client_secret(&client_secret)
                .map_err(|error| error.to_string())?;
            true
        }
        None => false,
    };
    tracing::info!("settings saved");
    apply_google(&app, &file);
    // A new secret can clear "sign in again" on every account.
    if has_new_secret && let Some(pipeline) = &state.pipeline {
        crate::check_google_accounts(app.clone(), std::sync::Arc::clone(pipeline));
    }
    Ok(())
}

/// Reads the `client_secret_*.json` that Google Cloud Console downloads.
#[tauri::command]
pub(crate) fn client_file_read(json: &str) -> Result<ClientFields, String> {
    let client = OAuthClient::from_client_file(json).map_err(|error| error.to_string())?;
    Ok(ClientFields {
        client_id: client.client_id,
        client_secret: client.client_secret,
    })
}

#[tauri::command]
pub(crate) async fn google_account_add(app: AppHandle) -> Result<String, String> {
    // Sign-in blocks on the browser for up to 5 minutes.
    tauri::async_runtime::spawn_blocking(move || add_account(&app))
        .await
        .map_err(|error| error.to_string())?
}

fn add_account(app: &AppHandle) -> Result<String, String> {
    let state = app.state::<AppState>();
    let file = SettingsFile::read(&state.app_dirs).map_err(|error| error.to_string())?;
    let client_id = file
        .google
        .client_id
        .clone()
        .ok_or("save a Google client ID first")?;
    let keychain = keychain(&state);
    let client = keychain
        .oauth_client(&client_id)
        .map_err(|error| error.to_string())?;
    let email = google::sign_in(&keychain, &client, |auth_url| {
        app.opener()
            .open_url(auth_url, None::<&str>)
            .map_err(|error| error.to_string())
    })
    .map_err(|error| {
        tracing::error!(exception.message = %error, "Google sign-in failed");
        error.to_string()
    })?;

    // Read again: the file may have changed during the minutes in the browser.
    let _settings_file = lock_settings_file(&state);
    let mut file = SettingsFile::read(&state.app_dirs).map_err(|error| error.to_string())?;
    if !file.google.accounts.contains(&email) {
        file.google.accounts.push(email.clone());
        file.write(&state.app_dirs)
            .map_err(|error| error.to_string())?;
    }
    if let Some(pipeline) = &state.pipeline {
        pipeline.google().replace(&file.google);
        pipeline.google().signed_in(&email);
    }
    crate::tray::refresh(app);
    Ok(email)
}

#[tauri::command]
pub(crate) async fn google_account_remove(app: AppHandle, email: String) -> Result<(), String> {
    // The revoke is a network call; keep it off the main thread.
    tauri::async_runtime::spawn_blocking(move || remove_account(&app, &email))
        .await
        .map_err(|error| error.to_string())?
}

fn remove_account(app: &AppHandle, email: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    let file = {
        let _settings_file = lock_settings_file(&state);
        let mut file = SettingsFile::read(&state.app_dirs).map_err(|error| error.to_string())?;
        file.google.accounts.retain(|account| account != email);
        file.write(&state.app_dirs)
            .map_err(|error| error.to_string())?;
        file
    };
    keychain(&state)
        .forget_account(email)
        .map_err(|error| error.to_string())?;
    tracing::info!(user.email = email, "Google account removed");
    apply_google(app, &file);
    Ok(())
}
