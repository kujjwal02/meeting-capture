//! The settings window: plain HTML in `ui/`, calling the commands here. It edits the settings
//! file and the keychain. Google account and LLM changes apply at once; the rest after a restart.

use std::path::PathBuf;
use std::sync::{MutexGuard, PoisonError};

use pipeline::calendar::AccountStatus;
use pipeline::google::{self, Keychain, OAuthClient};
use pipeline::llm::claude_code_auth;
use pipeline::settings::{ProviderKind, SettingsFile};
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
    llm: LlmView,
    settings_file: PathBuf,
}

#[derive(Serialize)]
pub(crate) struct LlmView {
    provider: Option<ProviderKind>,
    model: Option<String>,
    base_url: Option<String>,
    has_anthropic_key: bool,
    has_openai_compatible_key: bool,
    anthropic_default_model: Option<&'static str>,
    claude_code_default_model: Option<&'static str>,
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
    llm_provider: Option<ProviderKind>,
    llm_model: String,
    llm_base_url: String,
    /// Stored for `llm_provider`. Blank keeps the stored key.
    llm_api_key: String,
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
    let keychain = keychain(&state);
    let has_key = |provider| {
        keychain
            .llm_api_key(provider)
            .map(|key| key.is_some())
            .map_err(|error| error.to_string())
    };
    let llm = LlmView {
        provider: file.llm.provider,
        model: file.llm.model.clone(),
        base_url: file.llm.base_url.clone(),
        has_anthropic_key: has_key(ProviderKind::Anthropic)?,
        has_openai_compatible_key: has_key(ProviderKind::OpenaiCompatible)?,
        anthropic_default_model: ProviderKind::Anthropic.default_model(),
        claude_code_default_model: ProviderKind::ClaudeCode.default_model(),
    };
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
        llm,
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
) -> Result<String, String> {
    let settings_file_guard = lock_settings_file(&state);
    let mut file = SettingsFile::read(&state.app_dirs).map_err(|error| error.to_string())?;
    let old_base_url = file.llm.base_url.clone();
    file.vault_dir = non_blank(&form.vault_dir).map(PathBuf::from);
    file.my_name = non_blank(&form.my_name);
    file.receiver_port = form.receiver_port;
    file.time_zone = non_blank(&form.time_zone);
    file.google.client_id = non_blank(&form.client_id);
    file.llm.provider = form.llm_provider;
    file.llm.model = non_blank(&form.llm_model);
    // The window shows the base URL only for openai-compatible, the one provider that takes it.
    file.llm.base_url = match form.llm_provider {
        Some(ProviderKind::OpenaiCompatible) => non_blank(&form.llm_base_url),
        Some(ProviderKind::Anthropic | ProviderKind::ClaudeCode) | None => None,
    };
    file.llm.validate().map_err(|error| error.to_string())?;
    let api_key = non_blank(&form.llm_api_key);
    let key_provider = match form.llm_provider {
        Some(provider @ (ProviderKind::Anthropic | ProviderKind::OpenaiCompatible)) => {
            Some(provider)
        }
        Some(ProviderKind::ClaudeCode) | None => None,
    };
    if let (Some(api_key), Some(provider)) = (&api_key, key_provider) {
        keychain(&state)
            .set_llm_api_key(provider, api_key)
            .map_err(|error| error.to_string())?;
    }
    // The stored key was for the old endpoint; sending it to a new host would leak it.
    let is_new_endpoint = form.llm_provider == Some(ProviderKind::OpenaiCompatible)
        && old_base_url.is_some()
        && old_base_url != file.llm.base_url;
    let is_key_removed = is_new_endpoint && api_key.is_none();
    if is_key_removed {
        keychain(&state)
            .delete_llm_api_key(ProviderKind::OpenaiCompatible)
            .map_err(|error| error.to_string())?;
    }
    file.write(&state.app_dirs)
        .map_err(|error| error.to_string())?;
    drop(settings_file_guard);
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
    let Some(pipeline) = &state.pipeline else {
        return Ok("Saved. The app is not running the pipeline; see the tray.".to_owned());
    };
    // A new secret can clear "sign in again" on every account.
    if has_new_secret {
        crate::check_google_accounts(app.clone(), std::sync::Arc::clone(pipeline));
    }
    let queued = pipeline
        .replace_llm(file.llm.clone())
        .map_err(|error| error.to_string())?;
    let saved = if is_key_removed {
        "Saved. The API key for the old base URL was removed; enter one for the new URL if it needs one."
    } else {
        "Saved."
    };
    if queued == 0 {
        return Ok(saved.to_owned());
    }
    crate::meetings::run_unfinished(&app, pipeline);
    crate::tray::refresh(&app);
    Ok(format!(
        "{saved} {queued} {} queued for notes.",
        if queued == 1 { "meeting" } else { "meetings" }
    ))
}

/// Who Claude Code is signed in as, and what its calls bill.
#[tauri::command]
pub(crate) async fn claude_code_check(app: AppHandle) -> Result<String, String> {
    // The CLI can take seconds to start.
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let file = SettingsFile::read(&state.app_dirs).map_err(|error| error.to_string())?;
        let auth =
            claude_code_auth(file.llm.claude_path.as_deref()).map_err(|error| error.to_string())?;
        let account = auth.email.as_deref().unwrap_or("an unknown account");
        Ok(if auth.is_on_plan() {
            let plan = auth.subscription_type.as_deref().unwrap_or("Claude");
            format!("Signed in as {account}. Calls use the {plan} plan.")
        } else if auth.logged_in {
            format!(
                "Signed in as {account} with {}. Calls bill that API account, not a plan.",
                auth.auth_method
            )
        } else {
            "Not signed in. Run claude in a terminal and type /login.".to_owned()
        })
    })
    .await
    .map_err(|error| error.to_string())?
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
