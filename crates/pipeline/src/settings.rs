//! The settings file and the app's folders. The tray app and `mc` both load settings here.

use std::path::{Path, PathBuf};

use directories::{ProjectDirs, UserDirs};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("the OS reports no home folder, so the app folders cannot be placed")]
    NoHomeDir,
    #[error("could not read settings file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("settings file {path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("settings field vault_dir: {path} is not a folder")]
    VaultDirMissing { path: PathBuf },
    #[error("settings field receiver_port: must be between 1 and 65535")]
    ReceiverPortZero,
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

const KEYCHAIN_SERVICE: &str = "com.ujjwalkumar.meetingcapture";

/// Where the app keeps its own files. None of them go into the vault.
#[derive(Debug, Clone)]
pub struct AppDirs {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl AppDirs {
    /// The per-user folders named after the Tauri app identifier. `MEETING_CAPTURE_HOME`, when
    /// set, holds both instead, so a dev run never touches the real job store.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError::NoHomeDir` when the OS reports no home folder.
    pub fn locate() -> Result<Self, SettingsError> {
        if let Some(home) = std::env::var_os("MEETING_CAPTURE_HOME") {
            let home = PathBuf::from(home);
            return Ok(Self {
                config_dir: home.join("config"),
                data_dir: home.join("data"),
            });
        }
        let project = ProjectDirs::from("com", "ujjwalkumar", "meetingcapture")
            .ok_or(SettingsError::NoHomeDir)?;
        Ok(Self {
            config_dir: project.config_dir().to_path_buf(),
            data_dir: project.data_dir().to_path_buf(),
        })
    }

    /// Creates the folders the pipeline writes into.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError::CreateDir` naming the folder that failed.
    pub fn create(&self) -> Result<(), SettingsError> {
        for path in [
            &self.config_dir,
            &self.payload_dir(),
            &self.note_staging_dir(),
            &self.log_dir(),
        ] {
            std::fs::create_dir_all(path).map_err(|source| SettingsError::CreateDir {
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }

    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }

    /// Overrides the built-in note template when present.
    #[must_use]
    pub fn note_template_file(&self) -> PathBuf {
        self.config_dir.join("note.md.j2")
    }

    /// The OS keychain service that holds this install's secrets. A dev run under
    /// `MEETING_CAPTURE_HOME` gets its own, so it never reads the real refresh tokens.
    #[must_use]
    pub fn keychain_service(&self) -> String {
        match std::env::var_os("MEETING_CAPTURE_HOME") {
            Some(home) => format!("{KEYCHAIN_SERVICE}.dev:{}", home.to_string_lossy()),
            None => KEYCHAIN_SERVICE.to_owned(),
        }
    }

    #[must_use]
    pub fn job_store_file(&self) -> PathBuf {
        self.data_dir.join("jobs.sqlite")
    }

    /// Raw inputs: TranscripTonic payloads as received.
    #[must_use]
    pub fn payload_dir(&self) -> PathBuf {
        self.data_dir.join("payloads")
    }

    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// The empty working folder for Claude Code, so it reads no project files.
    #[must_use]
    pub fn claude_code_dir(&self) -> PathBuf {
        self.data_dir.join("claude-code")
    }

    /// Notes are written here first, then renamed into the vault, so Syncthing never sees a
    /// half-written note.
    #[must_use]
    pub fn note_staging_dir(&self) -> PathBuf {
        self.data_dir.join("note-staging")
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub vault_dir: PathBuf,
    /// Replaces "You", the name TranscripTonic gives the local user.
    pub my_name: Option<String>,
    pub receiver_port: u16,
    /// Note names, dates and the times the tray and `mc` show. Stored times stay UTC.
    pub time_zone: TimeZone,
    pub google: GoogleSettings,
    pub llm: LlmSettings,
}

/// The LLM that writes the notes. Validated once on load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSettings {
    /// No provider: extraction is off and notes carry only the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    /// The provider's default when absent; required for `openai-compatible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `openai-compatible` only, for example `https://openrouter.ai/api/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `claude-code` only. Claude Code's own default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Names and terms the captions often mishear, given to the model to spell right.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub glossary: Vec<String>,
    /// `claude-code` only: the `claude` binary, when it is not in a standard install folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// The Anthropic API, with a key from the keychain.
    Anthropic,
    /// Any endpoint that speaks the `OpenAI` Chat Completions API, such as `OpenRouter` or Ollama.
    OpenaiCompatible,
    /// A signed-in Claude Code, billed to the user's Claude plan.
    ClaudeCode,
}

impl ProviderKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenaiCompatible => "openai-compatible",
            Self::ClaudeCode => "claude-code",
        }
    }

    /// The model used when the settings name none.
    #[must_use]
    pub fn default_model(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("claude-haiku-5-5"),
            Self::ClaudeCode => Some("sonnet"),
            Self::OpenaiCompatible => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleSettings {
    /// The "Desktop app" OAuth client. Its secret is in the OS keychain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Signed-in accounts, in tie-break order: on an equal calendar match the first one wins.
    #[serde(default)]
    pub accounts: Vec<String>,
}

/// The settings file as written, before defaults fill the gaps. The settings window edits this.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vault_dir: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub my_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub google: GoogleSettings,
    #[serde(default)]
    pub llm: LlmSettings,
}

const DEFAULT_RECEIVER_PORT: u16 = 47615;
const GOOGLE_CLIENT_ID_SUFFIX: &str = ".apps.googleusercontent.com";

/// Heads the settings file whenever the app writes it.
const SETTINGS_HEADER: &str = "\
# Meeting Capture settings. The Settings window writes this file; quit and reopen the app after
# a hand edit.
#
# vault_dir: the Obsidian vault. Notes go under areas/meetings. Default ~/knowledge.
# my_name: replaces \"You\" in TranscripTonic transcripts.
# receiver_port: the port in TranscripTonic's webhook URL. Default 47615.
# time_zone: the IANA time zone for note names and shown times. Default: the device's.
# [google] client_id: the Desktop app OAuth client. Its secret is in the OS keychain.
# [google] accounts: signed-in accounts. On a tied calendar match the first one wins.
# [llm] provider: anthropic, openai-compatible or claude-code. Absent: notes carry no summary.
# [llm] model: defaults to claude-haiku-5-5 (anthropic) or sonnet (claude-code).
# [llm] base_url: the openai-compatible endpoint. API keys are in the OS keychain.
# [llm] effort: low, medium or high, claude-code only.
# [llm] glossary: names and terms the captions often mishear.
# [llm] claude_path: the claude binary, when it is not in a standard install folder.

";

impl SettingsFile {
    /// Reads the settings file. A missing file reads as empty.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the file.
    pub fn read(app_dirs: &AppDirs) -> Result<Self, SettingsError> {
        let path = app_dirs.settings_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|source| SettingsError::Parse { path, source })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SettingsError::Read { path, source }),
        }
    }

    /// Validates the settings, then replaces the file in one rename.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the invalid field or the file.
    pub fn write(&self, app_dirs: &AppDirs) -> Result<(), SettingsError> {
        Settings::resolve(self.clone())?;
        let path = app_dirs.settings_file();
        let write_error = |source| SettingsError::Write {
            path: path.clone(),
            source,
        };
        let body =
            toml::to_string(self).map_err(|error| SettingsError::Serialize(error.to_string()))?;
        let mut staged =
            tempfile::NamedTempFile::new_in(&app_dirs.config_dir).map_err(write_error)?;
        std::io::Write::write_all(&mut staged, format!("{SETTINGS_HEADER}{body}").as_bytes())
            .map_err(write_error)?;
        staged.as_file().sync_all().map_err(write_error)?;
        staged
            .persist(&path)
            .map_err(|error| write_error(error.error))?;
        Ok(())
    }
}

impl Settings {
    /// Loads and validates the settings file. A missing file gives the defaults.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the file or the invalid field.
    pub fn load(app_dirs: &AppDirs) -> Result<Self, SettingsError> {
        Self::resolve(SettingsFile::read(app_dirs)?)
    }

    /// Fills defaults into `file` and validates every field.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the invalid field.
    pub fn resolve(file: SettingsFile) -> Result<Self, SettingsError> {
        let vault_dir = match file.vault_dir {
            Some(vault_dir) => vault_dir,
            None => UserDirs::new()
                .ok_or(SettingsError::NoHomeDir)?
                .home_dir()
                .join("knowledge"),
        };
        if !vault_dir.is_dir() {
            return Err(SettingsError::VaultDirMissing { path: vault_dir });
        }
        if file
            .my_name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(SettingsError::BlankMyName);
        }
        let time_zone = match file.time_zone {
            Some(name) => {
                TimeZone::get(&name).map_err(|_| SettingsError::UnknownTimeZone { name })?
            }
            None => TimeZone::try_system().map_err(|error| SettingsError::NoDeviceTimeZone {
                reason: error.to_string(),
            })?,
        };
        if file.receiver_port == Some(0) {
            return Err(SettingsError::ReceiverPortZero);
        }
        if let Some(client_id) = &file.google.client_id
            && !client_id.ends_with(GOOGLE_CLIENT_ID_SUFFIX)
        {
            return Err(SettingsError::GoogleClientId {
                client_id: client_id.clone(),
            });
        }
        for (index, email) in file.google.accounts.iter().enumerate() {
            if !email.contains('@') || file.google.accounts[..index].contains(email) {
                return Err(SettingsError::GoogleAccount {
                    email: email.clone(),
                });
            }
        }
        file.llm.validate()?;
        Ok(Self {
            vault_dir,
            my_name: file.my_name,
            receiver_port: file.receiver_port.unwrap_or(DEFAULT_RECEIVER_PORT),
            time_zone,
            google: file.google,
            llm: file.llm,
        })
    }

    #[must_use]
    pub fn meetings_dir(&self) -> PathBuf {
        self.vault_dir.join(Path::new("areas").join("meetings"))
    }
}

// Each word costs prompt tokens on every call, and past about a hundred they stop helping.
const MAX_GLOSSARY_ENTRIES: usize = 100;

impl LlmSettings {
    /// # Errors
    ///
    /// Returns `SettingsError` naming the first invalid `llm` field.
    pub fn validate(&self) -> Result<(), SettingsError> {
        let llm = self;
        if llm
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(SettingsError::BlankLlmModel);
        }
        if llm.glossary.len() > MAX_GLOSSARY_ENTRIES
            || llm.glossary.iter().any(|entry| entry.trim().is_empty())
        {
            return Err(SettingsError::LlmGlossary {
                max: MAX_GLOSSARY_ENTRIES,
            });
        }
        let Some(provider) = llm.provider else {
            return Ok(());
        };
        let unused = |field| SettingsError::LlmFieldUnused {
            field,
            provider: provider.as_str(),
        };
        let missing = |field| SettingsError::LlmFieldMissing {
            field,
            provider: provider.as_str(),
        };
        match provider {
            ProviderKind::Anthropic | ProviderKind::OpenaiCompatible => {
                if llm.effort.is_some() {
                    return Err(unused("effort"));
                }
                if llm.claude_path.is_some() {
                    return Err(unused("claude_path"));
                }
            }
            ProviderKind::ClaudeCode => {
                if let Some(path) = &llm.claude_path {
                    if !path.is_file() {
                        return Err(SettingsError::ClaudePath {
                            path: path.clone(),
                            hint: "",
                        });
                    }
                    // npm's .cmd shim needs cmd.exe, which reparses the arguments.
                    if path.extension().is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("cmd")
                            || extension.eq_ignore_ascii_case("bat")
                    }) {
                        return Err(SettingsError::ClaudePath {
                            path: path.clone(),
                            hint: "; use claude.exe from the native installer, not the npm .cmd shim",
                        });
                    }
                }
            }
        }
        match provider {
            ProviderKind::OpenaiCompatible => {
                let url = llm.base_url.as_deref().ok_or_else(|| missing("base_url"))?;
                // The transcript and the API key travel in the request, so plain http is for a
                // local server only.
                let is_allowed =
                    reqwest::Url::parse(url).is_ok_and(|parsed| match parsed.scheme() {
                        "https" => true,
                        "http" => {
                            matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
                        }
                        _ => false,
                    });
                if !is_allowed {
                    return Err(SettingsError::LlmBaseUrl {
                        url: url.to_owned(),
                    });
                }
                if llm.model.is_none() {
                    return Err(missing("model"));
                }
            }
            ProviderKind::Anthropic | ProviderKind::ClaudeCode => {
                if llm.base_url.is_some() {
                    return Err(unused("base_url"));
                }
            }
        }
        Ok(())
    }
}
