use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AppDirs, Settings, SettingsError};

/// Slack huddle captions, read from the Slack desktop app. The app reads these before each
/// huddle, so a change applies to the next one without a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SlackSettings {
    pub read_captions: bool,
    /// Press the huddle's Captions tab when captions are on but the panel is hidden.
    pub auto_start_captions: bool,
    /// A shorter huddle writes no note; `mc ingest` can still write one from its log.
    pub min_huddle_seconds: u32,
}

impl Default for SlackSettings {
    fn default() -> Self {
        Self {
            read_captions: true,
            auto_start_captions: true,
            min_huddle_seconds: 120,
        }
    }
}

/// The LLM that writes the notes. Validated on load and on every write. The provider and base
/// URL change only through `set_endpoint`, which also decides which endpoint keeps a key.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSettings {
    /// No provider: extraction is off and notes carry only the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderKind>,
    /// The provider's default when absent; required for `openai-compatible`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `openai-compatible` only, for example `https://openrouter.ai/api/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// `claude-code` only. Claude Code's own default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Names and terms the captions often mishear, given to the model to spell right.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub glossary: Vec<String>,
    /// `claude-code` only: the `claude` binary, when it is not in a standard install folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_path: Option<PathBuf>,
    /// The key for `provider`, `anthropic` or `openai-compatible` only. A local server needs none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

// A stray `{:?}` in a log line must not print a secret.
fn secret_marker(secret: Option<&String>) -> Option<&'static str> {
    secret.map(|_| "<set>")
}

impl std::fmt::Debug for LlmSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlmSettings")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("effort", &self.effort)
            .field("glossary", &self.glossary)
            .field("claude_path", &self.claude_path)
            .field("api_key", &secret_marker(self.api_key.as_ref()))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// The Anthropic API, with a key from the settings file.
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

    /// Whether the provider sends `api_key`. Claude Code signs in on its own.
    #[must_use]
    pub fn takes_api_key(self) -> bool {
        match self {
            Self::Anthropic | Self::OpenaiCompatible => true,
            Self::ClaudeCode => false,
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

/// The client ID changes only through `set_client_id`, which drops the secret and the grants of
/// the old client.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleSettings {
    /// The "Desktop app" OAuth client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Signed-in accounts, in tie-break order: on an equal calendar match the first one wins.
    #[serde(default)]
    pub accounts: Vec<String>,
    /// One refresh token per signed-in account, by email.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refresh_tokens: BTreeMap<String, String>,
}

impl std::fmt::Debug for GoogleSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleSettings")
            .field("client_id", &self.client_id)
            .field("client_secret", &secret_marker(self.client_secret.as_ref()))
            .field("accounts", &self.accounts)
            .field(
                "refresh_tokens",
                &self
                    .refresh_tokens
                    .keys()
                    .map(|email| (email, "<set>"))
                    .collect::<BTreeMap<_, _>>(),
            )
            .finish()
    }
}

/// The settings file as written, before defaults fill the gaps. Edits go through
/// `SettingsStore::edit`.
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
    #[serde(default)]
    pub slack: SlackSettings,
}

/// Heads the settings file whenever the app writes it.
const SETTINGS_HEADER: &str = "\
# Meeting Capture settings. The Settings window writes this file; quit and reopen the app after
# a hand edit. This file holds secrets: keep it out of the vault and out of git.
#
# vault_dir: the Obsidian vault. Notes go under areas/meetings. Default ~/knowledge.
# my_name: replaces \"You\" in TranscripTonic transcripts.
# receiver_port: the port in TranscripTonic's webhook URL. Default 47615.
# time_zone: the IANA time zone for note names and shown times. Default: the device's.
# [google] client_id, client_secret: the Desktop app OAuth client.
# [google] accounts: signed-in accounts. On a tied calendar match the first one wins.
# [google] refresh_tokens: one per account, written by sign-in. Removing one signs it out.
# [llm] provider: anthropic, openai-compatible or claude-code. Absent: notes carry no summary.
# [llm] model: defaults to claude-haiku-5-5 (anthropic) or sonnet (claude-code).
# [llm] base_url: the openai-compatible endpoint.
# [llm] api_key: the key for the provider, anthropic or openai-compatible.
# [llm] effort: low, medium or high, claude-code only.
# [llm] glossary: names and terms the captions often mishear.
# [llm] claude_path: the claude binary, when it is not in a standard install folder.
# [slack] read_captions: read Slack huddle captions from the Slack desktop app (macOS).
# [slack] auto_start_captions: press the huddle's Captions tab when its caption panel is hidden.
# [slack] min_huddle_seconds: a shorter huddle writes no note. Default 120.

";

// Line and column are 1-based, the column in characters, as editors count them.
fn parse_error(path: PathBuf, text: &str, error: &toml::de::Error) -> SettingsError {
    let offset = error.span().map_or(0, |span| span.start.min(text.len()));
    let before = text.get(..offset).unwrap_or_default();
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    SettingsError::Parse {
        path,
        line: before.matches('\n').count() + 1,
        column: before[line_start..].chars().count() + 1,
        message: error.message().trim_end().to_owned(),
    }
}

impl SettingsFile {
    /// Reads the settings file. A missing file reads as empty.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the file.
    pub(super) fn read(app_dirs: &AppDirs) -> Result<Self, SettingsError> {
        let path = app_dirs.settings_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|error| parse_error(path, &text, &error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(SettingsError::Read { path, source }),
        }
    }

    /// Validates the settings, then replaces the file in one rename.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the invalid field or the file.
    pub(super) fn write(&self, app_dirs: &AppDirs) -> Result<(), SettingsError> {
        Settings::resolve(self)?;
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
        // The file holds secrets. On unix tempfile creates it as 0600 and the rename keeps that.
        staged
            .persist(&path)
            .map_err(|error| write_error(error.error))?;
        Ok(())
    }
}

impl LlmSettings {
    /// `None` when extraction is off.
    #[must_use]
    pub fn provider(&self) -> Option<ProviderKind> {
        self.provider
    }

    /// The `openai-compatible` endpoint.
    #[must_use]
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    /// Points the settings at another provider or base URL. Only `openai-compatible` keeps a
    /// base URL, and only a provider that takes a key keeps one. The stored key was for the old
    /// endpoint, and sending it to a new host would leak it, so a change drops it.
    pub fn set_endpoint(&mut self, provider: Option<ProviderKind>, base_url: Option<String>) {
        let base_url = base_url.filter(|_| provider == Some(ProviderKind::OpenaiCompatible));
        if self.provider != provider
            || self.base_url != base_url
            || !provider.is_some_and(ProviderKind::takes_api_key)
        {
            self.api_key = None;
        }
        self.provider = provider;
        self.base_url = base_url;
    }

    /// `<provider>/<model>`, or `None` when extraction is off.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        let provider = self.provider?;
        let model = self.model.as_deref().or(provider.default_model())?;
        Some(format!("{}/{model}", provider.as_str()))
    }
}

impl GoogleSettings {
    #[must_use]
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    /// Google issued the stored secret and grants to the old client, and they fail with a new
    /// one, so a new client ID drops the secret and every refresh token.
    pub fn set_client_id(&mut self, client_id: Option<String>) {
        if self.client_id != client_id {
            self.client_secret = None;
            self.refresh_tokens.clear();
        }
        self.client_id = client_id;
    }
}
