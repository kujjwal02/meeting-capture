//! The settings window: plain HTML in `ui/`, calling the commands here. It edits the settings
//! file through `SettingsStore`. Google account, LLM, Slack and Meet changes apply at once; the
//! rest after a restart.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pipeline::calendar::AccountStatus;
use pipeline::google::{self, OAuthClient};
use pipeline::llm::claude_code_auth;
use pipeline::settings::{MeetSettings, ProviderKind, SettingsError, SettingsFile, SlackSettings};
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
    time_zone: Option<String>,
    client_id: Option<String>,
    has_client_secret: bool,
    accounts: Vec<AccountView>,
    llm: LlmView,
    slack: SlackSettings,
    meet: MeetSettings,
    settings_file: PathBuf,
}

#[derive(Serialize)]
pub(crate) struct LlmView {
    provider: Option<ProviderKind>,
    model: Option<String>,
    base_url: Option<String>,
    /// The view never carries a secret, only whether one is stored.
    has_api_key: bool,
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
    time_zone: String,
    client_id: String,
    client_secret: String,
    llm_provider: Option<ProviderKind>,
    llm_model: String,
    llm_base_url: String,
    /// For `llm_provider`. Blank keeps the stored key while the provider and base URL stay.
    llm_api_key: String,
    slack_read_captions: bool,
    slack_auto_start_captions: bool,
    meet_hide_captions: bool,
}

#[derive(Serialize)]
pub(crate) struct ClientFields {
    client_id: String,
    client_secret: String,
}

fn account_statuses(state: &AppState, file: &SettingsFile) -> Vec<AccountStatus> {
    if state.pipeline.is_some() {
        return state.settings.google().statuses();
    }
    // Without a running pipeline nothing has checked the accounts, and after a failed load the
    // store holds none of them.
    file.google
        .accounts
        .iter()
        .map(|email| AccountStatus {
            email: email.clone(),
            needs_sign_in: false,
        })
        .collect()
}

fn non_blank(text: &str) -> Option<String> {
    Some(text.trim().to_owned()).filter(|text| !text.is_empty())
}

impl SettingsForm {
    fn apply_to(&self, file: &mut SettingsFile) {
        file.vault_dir = non_blank(&self.vault_dir).map(PathBuf::from);
        file.my_name = non_blank(&self.my_name);
        file.time_zone = non_blank(&self.time_zone);
        file.google.set_client_id(non_blank(&self.client_id));
        if let Some(client_secret) = non_blank(&self.client_secret) {
            file.google.client_secret = Some(client_secret);
        }
        file.llm
            .set_endpoint(self.llm_provider, non_blank(&self.llm_base_url));
        file.llm.model = non_blank(&self.llm_model);
        if self.llm_provider.is_some_and(ProviderKind::takes_api_key)
            && let Some(api_key) = non_blank(&self.llm_api_key)
        {
            file.llm.api_key = Some(api_key);
        }
        file.slack.read_captions = self.slack_read_captions;
        file.slack.auto_start_captions = self.slack_auto_start_captions;
        file.meet.hide_captions = self.meet_hide_captions;
    }
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri passes command arguments by value"
)]
pub(crate) fn settings_load(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let file = state.settings.read().map_err(|error| error.to_string())?;
    let llm = LlmView {
        provider: file.llm.provider(),
        model: file.llm.model.clone(),
        base_url: file.llm.base_url().map(str::to_owned),
        has_api_key: file.llm.api_key.is_some(),
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
        time_zone: file.time_zone,
        has_client_secret: file.google.client_secret.is_some(),
        client_id: file.google.client_id().map(str::to_owned),
        accounts,
        llm,
        slack: file.slack,
        meet: file.meet,
        settings_file: state.settings.app_dirs().settings_file(),
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
    let mut has_google_accounts = false;
    let change = state
        .settings
        .edit(|file| {
            form.apply_to(file);
            has_google_accounts = !file.google.accounts.is_empty();
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    tracing::info!("settings saved");
    crate::tray::refresh(&app);
    state.browser_calls.send_settings(state.settings.meet());
    revoke_in_background(change.tokens_to_revoke);
    let mut saved = "Saved.".to_owned();
    if change.has_dropped_llm_api_key && form.llm_provider.is_some_and(ProviderKind::takes_api_key)
    {
        saved.push_str(" The API key for the old provider or base URL was removed; enter one for the new one if it needs one.");
    }
    if change.has_dropped_google_client_secret {
        saved.push_str(
            " The client secret for the old Google client ID was removed; enter the new one.",
        );
    }
    if change.has_new_google_client && has_google_accounts {
        saved
            .push_str(" The Google client ID changed, so every Google account must sign in again.");
    }
    let Some(pipeline) = &state.pipeline else {
        return Ok(format!(
            "{saved} The app is not running the pipeline; see the tray."
        ));
    };
    // A typed secret can clear "sign in again" on every account, and a new client sets it.
    if change.has_new_google_client || non_blank(&form.client_secret).is_some() {
        crate::check_google_accounts(app.clone());
    }
    let queued = pipeline
        .queue_missing_extractions()
        .map_err(|error| error.to_string())?;
    if queued == 0 {
        return Ok(saved);
    }
    crate::meetings::run_unfinished(&app, pipeline);
    crate::tray::refresh(&app);
    Ok(format!(
        "{saved} {queued} {} queued for notes.",
        if queued == 1 { "meeting" } else { "meetings" }
    ))
}

// The revokes are network calls; a failed one is logged and the token is gone anyway.
fn revoke_in_background(tokens_to_revoke: BTreeMap<String, String>) {
    if tokens_to_revoke.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        for (email, refresh_token) in &tokens_to_revoke {
            google::revoke(email, refresh_token);
        }
    });
}

/// Who Claude Code is signed in as, and what its calls bill.
#[tauri::command]
pub(crate) async fn claude_code_check(app: AppHandle) -> Result<String, String> {
    // The CLI can take seconds to start.
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let file = state.settings.read().map_err(|error| error.to_string())?;
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
    let file = state.settings.read().map_err(|error| error.to_string())?;
    let client = OAuthClient::from_settings(&file.google)
        .map_err(|error| error.to_string())?
        .ok_or("save a Google client ID first")?;
    let signed_in = google::sign_in(&client, |auth_url| {
        app.opener()
            .open_url(auth_url, None::<&str>)
            .map_err(|error| error.to_string())
    })
    .map_err(|error| {
        tracing::error!(exception.message = %error, "Google sign-in failed");
        error.to_string()
    })?;

    // The edit reads the file again: it may have changed during the minutes in the browser.
    let email = signed_in.email;
    let refresh_token = signed_in.refresh_token;
    let edited = state.settings.edit(|file| {
        // The token belongs to the client that signed in, and fails with any other.
        if file.google.client_id() != Some(client.client_id.as_str()) {
            return Err(SettingsError::GoogleClientChangedDuringSignIn);
        }
        if !file.google.accounts.contains(&email) {
            file.google.accounts.push(email.clone());
        }
        file.google
            .refresh_tokens
            .insert(email.clone(), refresh_token.clone());
        Ok(())
    });
    match edited {
        // A sign-in adds a token and drops none, so nothing needs a revoke.
        Ok(_sign_in_change) => {}
        Err(error) => {
            // No stored token shares the grant: the client change dropped them all.
            if let SettingsError::GoogleClientChangedDuringSignIn = error {
                google::revoke(&email, &refresh_token);
            }
            return Err(error.to_string());
        }
    }
    state.settings.google().signed_in(&email);
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
    let change = state
        .settings
        .edit(|file| {
            file.google.accounts.retain(|account| account != email);
            file.google.refresh_tokens.remove(email);
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    crate::tray::refresh(app);
    tracing::info!(user.email = email, "Google account removed");
    for (email, refresh_token) in &change.tokens_to_revoke {
        google::revoke(email, refresh_token);
    }
    Ok(())
}
