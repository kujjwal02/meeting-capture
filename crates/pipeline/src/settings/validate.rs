use std::path::{Path, PathBuf};

use directories::UserDirs;
use jiff::tz::TimeZone;

use super::{LlmSettings, ProviderKind, SettingsError, SettingsFile, TranscriptionSettings};

/// The settings that apply at launch. LLM, transcription, Google, Slack and Meet settings change
/// while the app runs, so `SettingsStore` holds them instead.
#[derive(Debug)]
pub struct Settings {
    pub vault_dir: PathBuf,
    /// Replaces "You", the name TranscripTonic gives the local user.
    pub my_name: Option<String>,
    /// Note names, dates and the times the tray and `mc` show. Stored times stay UTC.
    pub time_zone: TimeZone,
}

const GOOGLE_CLIENT_ID_SUFFIX: &str = ".apps.googleusercontent.com";

impl Settings {
    /// Fills defaults into `file` and validates every field, the live sections included.
    pub(super) fn resolve(file: &SettingsFile) -> Result<Self, SettingsError> {
        let vault_dir = match &file.vault_dir {
            Some(vault_dir) => vault_dir.clone(),
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
        let time_zone = match &file.time_zone {
            Some(name) => TimeZone::get(name)
                .map_err(|_| SettingsError::UnknownTimeZone { name: name.clone() })?,
            None => TimeZone::try_system().map_err(|error| SettingsError::NoDeviceTimeZone {
                reason: error.to_string(),
            })?,
        };
        if let Some(client_id) = file.google.client_id()
            && !client_id.ends_with(GOOGLE_CLIENT_ID_SUFFIX)
        {
            return Err(SettingsError::GoogleClientId {
                client_id: client_id.to_owned(),
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
        file.transcription.validate()?;
        file.debug.mode()?;
        Ok(Self {
            vault_dir,
            my_name: file.my_name.clone(),
            time_zone,
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
        if self
            .model
            .as_deref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(SettingsError::BlankLlmModel);
        }
        if self.glossary.len() > MAX_GLOSSARY_ENTRIES
            || self.glossary.iter().any(|entry| entry.trim().is_empty())
        {
            return Err(SettingsError::LlmGlossary {
                max: MAX_GLOSSARY_ENTRIES,
            });
        }
        let Some(provider) = self.provider() else {
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
                if self.effort.is_some() {
                    return Err(unused("effort"));
                }
                if self.claude_path.is_some() {
                    return Err(unused("claude_path"));
                }
            }
            ProviderKind::ClaudeCode => {
                if self.api_key.is_some() {
                    return Err(unused("api_key"));
                }
                if let Some(path) = &self.claude_path {
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
                let url = self.base_url().ok_or_else(|| missing("base_url"))?;
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
                if self.model.is_none() {
                    return Err(missing("model"));
                }
            }
            ProviderKind::Anthropic | ProviderKind::ClaudeCode => {
                if self.base_url().is_some() {
                    return Err(unused("base_url"));
                }
            }
        }
        Ok(())
    }
}

fn is_model_name(model: &str) -> bool {
    model.starts_with(|character: char| character.is_ascii_alphanumeric())
        && model.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || character == '.'
        })
}

/// `multi`, or a BCP 47 tag of a 2- or 3-letter language and optional subtags.
fn is_language_code(language: &str) -> bool {
    if language == "multi" {
        return true;
    }
    let mut parts = language.split('-');
    let primary_is_valid = parts.next().is_some_and(|primary| {
        (2..=3).contains(&primary.len())
            && primary
                .chars()
                .all(|character| character.is_ascii_lowercase())
    });
    primary_is_valid
        && parts.all(|subtag| {
            (2..=8).contains(&subtag.len())
                && subtag
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
}

impl TranscriptionSettings {
    /// # Errors
    ///
    /// Returns `SettingsError` naming the first invalid `transcription` field.
    pub fn validate(&self) -> Result<(), SettingsError> {
        for (field, value) in [
            ("api_key", &self.api_key),
            ("model", &self.model),
            ("language", &self.language),
        ] {
            if value
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err(SettingsError::BlankTranscriptionField { field });
            }
        }
        // Both go into the request's query, and a typo would fail every request.
        if let Some(model) = self.model.as_deref()
            && !is_model_name(model)
        {
            return Err(SettingsError::TranscriptionFieldShape {
                field: "model",
                value: model.to_owned(),
                expected: "a model name such as \"nova-3\": lowercase letters, digits, '.' and '-'",
            });
        }
        if let Some(language) = self.language.as_deref()
            && !is_language_code(language)
        {
            return Err(SettingsError::TranscriptionFieldShape {
                field: "language",
                value: language.to_owned(),
                expected: "\"multi\" or a language code such as \"en\", \"hi\" or \"en-IN\"",
            });
        }
        match self.provider {
            None => Ok(()),
            Some(provider @ stt::ProviderKind::Deepgram) => {
                if self.api_key.is_none() {
                    return Err(SettingsError::TranscriptionFieldMissing {
                        field: "api_key",
                        provider: provider.as_str(),
                    });
                }
                Ok(())
            }
        }
    }
}
