//! The LLM call behind extraction: a closed set of providers, their timeouts, and the repair loop
//! that sends a reply's problems back until it fits.

mod api;
mod claude_code;

pub use claude_code::{ClaudeCodeAuth, claude_code_auth};

use crate::extraction::{self, CheckedNotes, MeetingNotes, Prompt};
use crate::settings::{AppDirs, LlmSettings, ProviderKind};

/// Why an extraction produced no notes. Each message names the provider and model.
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    #[error("Claude Code is not installed: no claude binary in {searched}; set llm.claude_path")]
    CliMissing { searched: String },
    #[error("{provider} is not signed in: {detail}")]
    NotSignedIn { provider: String, detail: String },
    #[error("{provider} refused the call: the rate or usage limit is reached")]
    RateLimited { provider: String },
    #[error("{provider} gave no reply within {limit_s} s")]
    Timeout { provider: String, limit_s: u64 },
    #[error("{provider} sent notes that still do not fit after {MAX_REPAIRS} repair turns: {}", errors.join("; "))]
    InvalidReply {
        provider: String,
        errors: Vec<String>,
    },
    #[error("{provider} failed: {detail}")]
    Transport { provider: String, detail: String },
    #[error("{provider} stopped at its output limit before the notes were complete")]
    Truncated { provider: String },
    #[error("no API key for {provider} in settings; add it in Settings")]
    NoApiKey { provider: &'static str },
}

impl ExtractionError {
    /// Whether trying again without a change from the user would fail the same way.
    #[must_use]
    pub fn is_lasting(&self) -> bool {
        match self {
            Self::CliMissing { .. }
            | Self::NotSignedIn { .. }
            | Self::NoApiKey { .. }
            | Self::Truncated { .. } => true,
            Self::RateLimited { .. }
            | Self::Timeout { .. }
            | Self::InvalidReply { .. }
            | Self::Transport { .. } => false,
        }
    }
}

/// Sent as the system prompt. The schema travels separately, as each provider takes it.
pub(crate) const SYSTEM_PROMPT: &str = "You are a meeting-notes extraction function, not a chat assistant. Reply with exactly one JSON object that matches the given JSON schema. No prose, no Markdown, no code fence.";

// Two turns fixed every bad reply in testing; more only burns plan usage.
const MAX_REPAIRS: usize = 2;

/// The configured LLM, built once from the settings.
#[derive(Debug, Clone)]
pub struct Provider {
    label: String,
    backend: Backend,
}

#[derive(Debug, Clone)]
enum Backend {
    Api(Box<api::ApiClient>),
    ClaudeCode(claude_code::ClaudeCode),
}

/// One reply, parsed or not.
pub(crate) enum Reply {
    Parsed(MeetingNotes),
    /// Why the reply is not a `MeetingNotes`, with the path of the field.
    Unparsed(String),
}

/// An open conversation with the model. Each `send` is one user turn.
pub(crate) trait Conversation {
    fn send(&mut self, message: &str) -> Result<Reply, ExtractionError>;
}

impl Provider {
    /// The provider the settings name, or `None` when extraction is off.
    ///
    /// # Panics
    ///
    /// Panics when `llm` skipped settings validation and lacks the base URL `openai-compatible`
    /// needs.
    ///
    /// # Errors
    ///
    /// Returns `ExtractionError` when the provider cannot be set up: no API key, no `claude`.
    pub fn from_settings(
        llm: &LlmSettings,
        app_dirs: &AppDirs,
    ) -> Result<Option<Self>, ExtractionError> {
        let (Some(kind), Some(label)) = (llm.provider, llm.label()) else {
            return Ok(None);
        };
        let model = llm
            .model
            .clone()
            .or_else(|| kind.default_model().map(str::to_owned))
            .unwrap_or_default();
        let backend = match kind {
            ProviderKind::Anthropic => Backend::Api(Box::new(api::ApiClient::new(
                api::Endpoint::Anthropic,
                model,
                llm.api_key.clone(),
            )?)),
            ProviderKind::OpenaiCompatible => Backend::Api(Box::new(api::ApiClient::new(
                api::Endpoint::OpenaiCompatible {
                    base_url: llm
                        .base_url
                        .clone()
                        .expect("settings validation requires llm.base_url for openai-compatible"),
                },
                model,
                llm.api_key.clone(),
            )?)),
            ProviderKind::ClaudeCode => {
                Backend::ClaudeCode(claude_code::ClaudeCode::new(llm, model, app_dirs)?)
            }
        };
        Ok(Some(Self { label, backend }))
    }

    /// `<provider>/<model>`, as stored with the notes and shown in errors.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Asks the model for notes, sends its mistakes back up to `MAX_REPAIRS` times, then checks
    /// every quote.
    ///
    /// # Errors
    ///
    /// Returns `ExtractionError` naming the provider when no fitting reply came.
    pub fn extract(&self, prompt: &Prompt) -> Result<CheckedNotes, ExtractionError> {
        let _span =
            tracing::info_span!("llm.extract", gen_ai.request.model = %self.label).entered();
        match &self.backend {
            Backend::Api(client) => self.converse(client.start(&self.label), prompt),
            Backend::ClaudeCode(cli) => self.converse(cli.start(&self.label)?, prompt),
        }
    }

    fn converse(
        &self,
        mut conversation: impl Conversation,
        prompt: &Prompt,
    ) -> Result<CheckedNotes, ExtractionError> {
        let mut reply = conversation.send(&prompt.render())?;
        let mut repairs = 0;
        loop {
            let errors = match reply {
                Reply::Parsed(mut notes) => {
                    let mut errors = extraction::reply_errors(&notes, prompt);
                    // The notes stand without a bad caption fix, so the last reply drops it
                    // rather than fail.
                    if repairs == MAX_REPAIRS {
                        let dropped = extraction::drop_bad_caption_fixes(&mut notes, prompt);
                        if !dropped.is_empty() {
                            tracing::warn!(
                                extraction.errors = %dropped.join("; "),
                                "caption fixes dropped from the last reply"
                            );
                        }
                    } else {
                        errors.extend(extraction::caption_fix_errors(&notes, prompt));
                    }
                    // The last reply is kept with its bad quotes dropped, unless that drops most
                    // items.
                    if errors.is_empty() && repairs < MAX_REPAIRS {
                        errors = extraction::quote_errors(&notes, prompt);
                    }
                    if errors.is_empty() {
                        return self.keep_checked(notes, prompt);
                    }
                    errors
                }
                Reply::Unparsed(error) => vec![format!("the reply is not valid: {error}")],
            };
            if repairs == MAX_REPAIRS {
                return Err(ExtractionError::InvalidReply {
                    provider: self.label.clone(),
                    errors,
                });
            }
            repairs += 1;
            tracing::warn!(
                extraction.repair = repairs,
                extraction.errors = %errors.join("; "),
                "reply does not fit; sending a repair turn"
            );
            reply = conversation.send(&format!(
                "Your reply failed validation:\n- {}\nReply again with the whole corrected JSON object only.",
                errors.join("\n- ")
            ))?;
        }
    }

    fn keep_checked(
        &self,
        notes: MeetingNotes,
        prompt: &Prompt,
    ) -> Result<CheckedNotes, ExtractionError> {
        let item_count = notes.item_count();
        let checked = extraction::check_citations(notes, prompt);
        let dropped_count = item_count - checked.item_count();
        if dropped_count * 2 > item_count {
            return Err(ExtractionError::InvalidReply {
                provider: self.label.clone(),
                errors: vec![format!(
                    "{dropped_count} of {item_count} items cite only quotes that are not in the transcript"
                )],
            });
        }
        Ok(checked)
    }
}
