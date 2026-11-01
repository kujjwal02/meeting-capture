//! The settings file and the app's folders. The tray app and `mc` both load settings here, and
//! every edit of the file goes through `SettingsStore`.

mod app_dirs;
mod edit;
mod file;
mod validate;

use std::path::PathBuf;

pub use app_dirs::AppDirs;
pub use edit::{SettingsChange, SettingsStore};
pub use file::{
    DebugSettings, Effort, GoogleSettings, LlmSettings, MeetSettings, ProviderKind, SettingsFile,
    SlackSettings,
};
pub use validate::Settings;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("the OS reports no home folder, so the app folders cannot be placed")]
    NoHomeDir,
    #[error("could not read settings file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Holds only the parser's message and position. The toml error's Display quotes the
    /// source line, which can hold the API key or client secret.
    #[error("settings file {path} is not valid at line {line}, column {column}: {message}")]
    Parse {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    #[error("settings field vault_dir: {path} is not a folder")]
    VaultDirMissing { path: PathBuf },
    #[error("settings field my_name: must not be blank")]
    BlankMyName,
    #[error(
        "settings field time_zone: {name:?} is not a time zone; use an IANA name such as \"Asia/Kolkata\" or \"Europe/London\""
    )]
    UnknownTimeZone { name: String },
    #[error(
        "settings field time_zone: the device time zone is unknown ({reason}); set an IANA name such as \"Asia/Kolkata\""
    )]
    NoDeviceTimeZone { reason: String },
    #[error(
        "settings field google.client_id: {client_id:?} is not a Google OAuth client ID; it ends in \".apps.googleusercontent.com\""
    )]
    GoogleClientId { client_id: String },
    #[error("settings field google.client_id: changed during the Google sign-in; sign in again")]
    GoogleClientChangedDuringSignIn,
    #[error("settings field google.accounts: {email:?} is not an email address or is listed twice")]
    GoogleAccount { email: String },
    #[error("settings field llm.{field}: required for provider {provider}")]
    LlmFieldMissing {
        field: &'static str,
        provider: &'static str,
    },
    #[error("settings field llm.{field}: provider {provider} does not use it; remove it")]
    LlmFieldUnused {
        field: &'static str,
        provider: &'static str,
    },
    #[error(
        "settings field llm.base_url: {url:?} must be an https URL, or http on localhost, 127.0.0.1 or [::1]"
    )]
    LlmBaseUrl { url: String },
    #[error("settings field llm.model: must not be blank")]
    BlankLlmModel,
    #[error(
        "settings field llm.glossary: entries must not be blank, and at most {max} are allowed"
    )]
    LlmGlossary { max: usize },
    #[error("settings field llm.claude_path: {path} is not a file{hint}")]
    ClaudePath { path: PathBuf, hint: &'static str },
    #[error(
        "settings field debug.enabled_until: {value:?} is not an RFC 3339 time such as \"2026-11-01T04:10:00Z\"; remove it to turn debug mode off"
    )]
    DebugEnabledUntil { value: String },
    #[error("could not write settings file {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write the settings: {0}")]
    Serialize(String),
    #[error("could not create app folder {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
}
