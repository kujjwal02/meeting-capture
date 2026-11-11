//! The `anthropic` and `openai-compatible` providers, through rig. rig is async and the pipeline
//! is not, so each call runs on its own current-thread tokio runtime.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rig_agent::agent::{AgentHook, HookContext, ModelTurnAction, ModelTurnFinished};
use rig_agent::completion::{FinishReason, Message, PromptError, StructuredOutputError};
use rig_agent::{Agent, AgentBuilder};
use rig_core::ErrorKind;
use rig_core::providers::anthropic::AnthropicConfig;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::providers::openai::wire::{Auth, OPENAI};

use super::{Conversation, ExtractionError, SYSTEM_PROMPT};
use crate::extraction::{MeetingNotes, Reply};
use crate::settings::ProviderKind;
use crate::transcribe::coherence::{self, CaptionJudgement};

// A 60-minute call's notes come back in well under a minute; a slow local model gets the rest.
const CALL_TIMEOUT: Duration = Duration::from_mins(3);
// The notes run to about 3k tokens on an 11k-word call. Anthropic needs a cap; rig's catalog has
// none for the 5.5 models.
const MAX_OUTPUT_TOKENS: u64 = 16_000;
// The caption judgement is one word in a JSON object.
const MAX_JUDGEMENT_TOKENS: u64 = 200;
const JUDGEMENT_TIMEOUT: Duration = Duration::from_mins(1);

pub(super) enum Endpoint {
    Anthropic,
    OpenaiCompatible { base_url: String },
}

#[derive(Clone)]
pub(super) struct ApiClient {
    agent: Agent,
    /// The same model with the caption judge's instructions.
    judge: Agent,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ApiClient").finish_non_exhaustive()
    }
}

impl ApiClient {
    pub(super) fn new(
        endpoint: &Endpoint,
        model: &str,
        api_key: Option<&str>,
    ) -> Result<Self, ExtractionError> {
        let builder = |preamble: &str, max_tokens: u64| -> Result<Agent, ExtractionError> {
            Ok(match endpoint {
                Endpoint::Anthropic => {
                    let api_key = api_key.ok_or(ExtractionError::NoApiKey {
                        provider: ProviderKind::Anthropic.as_str(),
                    })?;
                    AgentBuilder::new(
                        AnthropicConfig::new(api_key.to_owned())
                            .client()
                            .completion(model.to_owned()),
                    )
                }
                Endpoint::OpenaiCompatible { base_url } => {
                    // Local servers such as Ollama take no key; an empty one sends no header.
                    let client =
                        OpenAIConfig::with_key(&OPENAI, api_key.unwrap_or_default().to_owned())
                            .with_base_url(base_url.clone())
                            .with_auth(Auth::OptionalBearer)
                            .client();
                    AgentBuilder::new(client.chat(model.to_owned()))
                }
            }
            .preamble(preamble)
            .temperature(0.0)
            .max_tokens(max_tokens)
            .build())
        };
        Ok(Self {
            agent: builder(SYSTEM_PROMPT, MAX_OUTPUT_TOKENS)?,
            judge: builder(coherence::SYSTEM_PROMPT, MAX_JUDGEMENT_TOKENS)?,
        })
    }

    /// Asks whether caption lines read as sensible English.
    pub(super) fn judge_captions(
        &self,
        label: &str,
        message: &str,
    ) -> Result<CaptionJudgement, ExtractionError> {
        let conversation = ApiConversation {
            agent: self.judge.clone(),
            history: Vec::new(),
            label: label.to_owned(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                conversation.transport(format!("could not start a runtime: {error}"))
            })?;
        let run = self
            .judge
            .prompt_typed::<CaptionJudgement>(message.to_owned());
        let outcome = runtime
            .block_on(async { tokio::time::timeout(JUDGEMENT_TIMEOUT, run).await })
            .map_err(|_| ExtractionError::Timeout {
                provider: label.to_owned(),
                limit_s: JUDGEMENT_TIMEOUT.as_secs(),
            })?;
        match outcome {
            Ok(response) => Ok(response.output),
            Err(StructuredOutputError::Deserialization { error, .. }) => {
                Err(conversation.transport(format!("the reply does not fit: {error}")))
            }
            Err(StructuredOutputError::EmptyResponse) => {
                Err(conversation.transport("the reply was empty".to_owned()))
            }
            Err(StructuredOutputError::Prompt(error)) => Err(conversation.prompt_error(&error)),
        }
    }

    pub(super) fn start(&self, label: &str) -> impl Conversation + use<> {
        ApiConversation {
            agent: self.agent.clone(),
            history: Vec::new(),
            label: label.to_owned(),
        }
    }
}

struct ApiConversation {
    agent: Agent,
    history: Vec<Message>,
    label: String,
}

/// Records why the model stopped, since a reply cut off at the token cap is not an error to rig.
#[derive(Clone, Default)]
struct FinishWatch(Arc<Mutex<Option<FinishReason>>>);

impl AgentHook for FinishWatch {
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "rig declares the hook async"
    )]
    async fn on_model_turn_finished(
        &self,
        _context: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if let Ok(mut finish) = self.0.lock() {
            *finish = event.finish_reason.cloned();
        }
        ModelTurnAction::continue_run()
    }
}

impl Conversation for ApiConversation {
    fn send(&mut self, message: &str) -> Result<Reply, ExtractionError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| self.transport(format!("could not start a runtime: {error}")))?;
        let finish = FinishWatch::default();
        let run = self
            .agent
            .prompt_typed::<MeetingNotes>(message.to_owned())
            .history(self.history.clone())
            .add_hook(finish.clone());
        let outcome = runtime
            .block_on(async { tokio::time::timeout(CALL_TIMEOUT, run).await })
            .map_err(|_| ExtractionError::Timeout {
                provider: self.label.clone(),
                limit_s: CALL_TIMEOUT.as_secs(),
            })?;
        let is_truncated = finish
            .0
            .lock()
            .ok()
            .and_then(|finish| finish.clone())
            .is_some_and(|reason| reason.truncated_output());
        if is_truncated {
            return Err(ExtractionError::Truncated {
                provider: self.label.clone(),
            });
        }
        match outcome {
            Ok(response) => {
                tracing::info!(
                    gen_ai.usage.input_tokens = response.usage.input_tokens.unwrap_or(0),
                    gen_ai.usage.output_tokens = response.usage.output_tokens.unwrap_or(0),
                    "llm call done"
                );
                self.history.extend(response.messages);
                Ok(Reply::Parsed(response.output))
            }
            Err(StructuredOutputError::Deserialization { output, error }) => {
                // rig returns no transcript for a reply it could not parse, so the turn is rebuilt
                // for the repair message to refer to.
                self.history.push(Message::user(message.to_owned()));
                self.history.push(Message::assistant(output));
                Ok(Reply::Unparsed(error.to_string()))
            }
            Err(StructuredOutputError::EmptyResponse) => {
                self.history.push(Message::user(message.to_owned()));
                Ok(Reply::Unparsed("the reply was empty".to_owned()))
            }
            Err(StructuredOutputError::Prompt(error)) => Err(self.prompt_error(&error)),
        }
    }
}

impl ApiConversation {
    fn prompt_error(&self, error: &PromptError) -> ExtractionError {
        let provider = self.label.clone();
        let status = error
            .provider_response_status()
            .map(|status| status.as_u16());
        let kind = match error {
            PromptError::Report(report) => Some(report.kind),
            PromptError::Provider(provider_error) => Some(provider_error.kind()),
            _ => None,
        };
        match (status, kind) {
            (Some(401 | 403), _) => ExtractionError::NotSignedIn {
                provider,
                // Some servers echo part of the key in the refusal, and this text is stored.
                detail: format!(
                    "the API key was refused (HTTP {}); replace it in Settings",
                    status.unwrap_or_default()
                ),
            },
            (Some(429), _) => ExtractionError::RateLimited { provider },
            (_, Some(ErrorKind::Timeout)) => ExtractionError::Timeout {
                provider,
                limit_s: CALL_TIMEOUT.as_secs(),
            },
            _ => ExtractionError::Transport {
                provider,
                detail: error.to_string(),
            },
        }
    }

    fn transport(&self, detail: String) -> ExtractionError {
        ExtractionError::Transport {
            provider: self.label.clone(),
            detail,
        }
    }
}
