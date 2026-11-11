//! Speech-to-text of stretches of a 16 kHz mono stem through one `SpeechToText` trait. Deepgram is
//! the only provider today; local models join later as more variants of `Provider`.
//!
//! Audio goes out as Ogg Opus, a tenth of the WAV's size. A provider returns its reply as it
//! came, so the caller can save it and read it again later with no second request.

mod deepgram;
pub mod ogg_opus;
#[cfg(feature = "wispr")]
mod wispr;

use serde::{Deserialize, Serialize};

pub use crate::deepgram::Deepgram;
pub use crate::ogg_opus::EncodedAudio;
#[cfg(feature = "wispr")]
pub use crate::wispr::Wispr;

/// The speech-to-text services the settings can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Deepgram's prerecorded API, with a key from the settings file.
    Deepgram,
    /// Wispr Flow's dictation API, with a token borrowed from the installed desktop app. Built
    /// only with the `wispr` feature.
    Wispr,
}

impl ProviderKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
            Self::Wispr => "wispr",
        }
    }

    /// The model used when the settings name none.
    #[must_use]
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Deepgram => "nova-3",
            // Wispr has no model to choose; this names it in labels only.
            Self::Wispr => "flow",
        }
    }

    /// The language used when the settings name none. `multi` covers Hindi-English
    /// code-switching.
    #[must_use]
    pub fn default_language(self) -> &'static str {
        match self {
            Self::Deepgram | Self::Wispr => "multi",
        }
    }

    /// Whether this provider reads its key from `transcription.api_key`. Wispr borrows the
    /// desktop app's token instead, so it needs none.
    #[must_use]
    pub fn requires_api_key(self) -> bool {
        match self {
            Self::Deepgram => true,
            Self::Wispr => false,
        }
    }

    /// Whether this provider can be used in this build on this machine: the engine is compiled
    /// in and anything it needs locally is present.
    #[must_use]
    pub fn is_available(self) -> bool {
        match self {
            Self::Deepgram => true,
            #[cfg(feature = "wispr")]
            Self::Wispr => wispr::is_installed(),
            #[cfg(not(feature = "wispr"))]
            Self::Wispr => false,
        }
    }

    /// A warning to show before this provider is turned on, if any.
    #[must_use]
    pub fn activation_warning(self) -> Option<&'static str> {
        match self {
            Self::Deepgram => None,
            Self::Wispr => Some(
                "Wispr Flow transcription is unsupported: it uses Wispr's own API with a token \
                 borrowed from the desktop app, against Wispr's terms of service, and may get your \
                 Wispr account flagged or banned. The desktop app must stay installed and signed \
                 in.",
            ),
        }
    }

    /// The providers that can be used in this build on this machine, for a settings menu.
    #[must_use]
    pub fn selectable() -> Vec<Self> {
        [Self::Deepgram, Self::Wispr]
            .into_iter()
            .filter(|kind| kind.is_available())
            .collect()
    }

    /// Reads the utterances from a reply this provider sent, now or in an earlier run. Needs no
    /// key, so a saved reply reads after the settings change.
    ///
    /// # Errors
    ///
    /// Returns `SttError::UnreadableReply` when the reply lacks what the provider always sends.
    pub fn utterances(self, reply: &serde_json::Value) -> Result<Vec<Utterance>, SttError> {
        match self {
            Self::Deepgram => deepgram::read_utterances(reply),
            #[cfg(feature = "wispr")]
            Self::Wispr => wispr::read_utterances(reply),
            #[cfg(not(feature = "wispr"))]
            Self::Wispr => Err(SttError::EngineNotBuilt { provider: self }),
        }
    }

    /// Each word of a language-check reply with its language tag, such as `en` or `hi`.
    ///
    /// # Errors
    ///
    /// Returns `SttError::UnreadableReply` when the reply lacks what the provider always sends.
    pub fn word_languages(self, reply: &serde_json::Value) -> Result<Vec<TaggedWord>, SttError> {
        match self {
            Self::Deepgram => deepgram::read_word_languages(reply),
            #[cfg(feature = "wispr")]
            Self::Wispr => wispr::read_word_languages(reply),
            #[cfg(not(feature = "wispr"))]
            Self::Wispr => Err(SttError::EngineNotBuilt { provider: self }),
        }
    }
}

/// Who speaks on a stem, which decides whether the provider separates speakers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemVoices {
    /// The mic stem: the local user alone.
    OnePerson,
    /// The remote stem: everyone else on the call.
    Several,
}

/// One stretch of speech, with seconds from the start of the audio sent.
#[derive(Debug, Clone)]
pub struct Utterance {
    pub start_s: f64,
    pub end_s: f64,
    /// The provider's speaker number within this request, when it separated speakers.
    pub speaker: Option<u32>,
    pub text: String,
    /// In order. Empty when the provider gave no word timings.
    pub words: Vec<Word>,
}

/// One word of an utterance, with seconds from the start of the audio sent.
#[derive(Debug, Clone)]
pub struct Word {
    pub start_s: f64,
    pub end_s: f64,
    /// As the transcript shows it, with its capitals and punctuation.
    pub text: String,
}

/// A word of a language-check reply, with seconds from the start of the audio sent.
#[derive(Debug, Clone)]
pub struct TaggedWord {
    pub start_s: f64,
    /// As the provider tags it, such as `en`, `hi` or `te`.
    pub language: String,
}

/// Whether a failed request can work later, and what the caller does about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// A dropped or slow network, a rate limit or a provider outage. Try again later.
    Passing,
    /// The provider refused the key, the account or the request. Retries fail the same way
    /// until the settings or the account change.
    Refused,
    /// The audio could not be read or encoded, or the reply could not be read.
    Local,
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("{provider} refused the API key (HTTP {status}); replace transcription.api_key")]
    KeyRefused { provider: String, status: u16 },
    #[error(
        "the {provider} API key holds characters an HTTP header cannot carry; replace transcription.api_key"
    )]
    KeyMalformed { provider: String },
    #[error("{provider} refused the request: the account is out of credit (HTTP 402)")]
    OutOfCredit { provider: String },
    #[error(
        "{provider} refused the request ({detail}); check transcription.model and transcription.language"
    )]
    RequestRejected { provider: String, detail: String },
    #[error("{provider} refused the call: the rate limit is reached")]
    RateLimited { provider: String },
    #[error("{provider} had a server error ({detail})")]
    ServerError { provider: String, detail: String },
    #[error("{provider} gave no reply within {limit_s} s")]
    Timeout { provider: String, limit_s: u64 },
    #[error("{provider} could not be reached: {detail}")]
    Network { provider: String, detail: String },
    #[error("could not read {path}: {source}")]
    ReadStem {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("could not encode the audio as Opus: {detail}")]
    Encode { detail: String },
    #[error("{provider} sent a reply this app cannot read: {detail}")]
    UnreadableReply { provider: String, detail: String },
    #[error("no Wispr Flow session: {detail}")]
    WisprSession { detail: String },
    #[error(
        "the {} engine is not built into this app; rebuild with its feature enabled",
        provider.as_str()
    )]
    EngineNotBuilt { provider: ProviderKind },
}

impl SttError {
    #[must_use]
    pub fn class(&self) -> FailureClass {
        match self {
            Self::RateLimited { .. }
            | Self::ServerError { .. }
            | Self::Timeout { .. }
            | Self::Network { .. } => FailureClass::Passing,
            Self::KeyRefused { .. }
            | Self::KeyMalformed { .. }
            | Self::OutOfCredit { .. }
            | Self::RequestRejected { .. }
            | Self::WisprSession { .. }
            | Self::EngineNotBuilt { .. } => FailureClass::Refused,
            Self::ReadStem { .. } | Self::Encode { .. } | Self::UnreadableReply { .. } => {
                FailureClass::Local
            }
        }
    }
}

/// One speech-to-text service.
pub trait SpeechToText {
    fn kind(&self) -> ProviderKind;

    /// `<provider>/<model>`, as the note and the logs name it.
    fn label(&self) -> String;

    /// Encodes a stretch of 16 kHz mono samples the way this provider wants it uploaded.
    ///
    /// # Errors
    ///
    /// Returns `SttError::Encode` when the samples cannot be encoded.
    fn encode(&self, samples: &[i16]) -> Result<EncodedAudio, SttError>;

    /// What a saved transcript reply must have been asked with to be reused: the provider,
    /// model, language, options for `voices` and the audio encoding.
    fn request_key(&self, voices: StemVoices) -> String;

    /// How long a request with `audio_bytes` of audio may take, upload and reply included.
    fn request_timeout(&self, audio_bytes: usize) -> std::time::Duration;

    /// What a saved language-check reply must have been asked with to be reused.
    fn language_check_key(&self, voices: StemVoices) -> String;

    /// Whether the provider's host takes a connection, with no request sent and nothing billed.
    /// A caller waiting for the network checks it before costly work for a request.
    ///
    /// # Errors
    ///
    /// Returns `SttError::Network` naming the provider when no connection opens.
    fn check_reachable(&self) -> Result<(), SttError>;

    /// Sends one Ogg Opus file of speech and returns the reply as the provider sent it. One
    /// try: the caller decides on retries from `SttError::class`.
    ///
    /// # Errors
    ///
    /// Returns `SttError` naming the provider when no reply came.
    fn transcribe(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError>;

    /// Transcribes one Ogg Opus file of speech in the provider's multilingual mode, whatever the
    /// settings' language, so each word comes back tagged with its language. The reply reads
    /// with `ProviderKind::utterances` too.
    ///
    /// # Errors
    ///
    /// Returns `SttError` naming the provider when no reply came.
    fn language_check(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError>;
}

/// The configured provider. A closed set, so each new provider is a variant here.
#[derive(Debug, Clone)]
pub enum Provider {
    Deepgram(Deepgram),
    #[cfg(feature = "wispr")]
    Wispr(Wispr),
}

impl Provider {
    /// The provider for `kind`, asking `model` in `language`. `api_key` is required for a
    /// provider whose `requires_api_key` is true.
    ///
    /// # Errors
    ///
    /// Returns `SttError::KeyRefused` when a key is needed but missing, `SttError::EngineNotBuilt`
    /// when the engine is not compiled in, or `SttError::Network` when the HTTP client cannot be
    /// built.
    pub fn new(
        kind: ProviderKind,
        api_key: Option<String>,
        model: String,
        language: String,
    ) -> Result<Self, SttError> {
        match kind {
            ProviderKind::Deepgram => {
                let api_key = api_key.ok_or(SttError::KeyRefused {
                    provider: kind.as_str().to_owned(),
                    status: 401,
                })?;
                Ok(Self::Deepgram(Deepgram::new(api_key, model, language)?))
            }
            #[cfg(feature = "wispr")]
            ProviderKind::Wispr => Ok(Self::Wispr(Wispr::new(&model, &language)?)),
            #[cfg(not(feature = "wispr"))]
            ProviderKind::Wispr => Err(SttError::EngineNotBuilt { provider: kind }),
        }
    }
}

/// Forwards each trait method to the chosen engine. The `wispr` arms compile only with the
/// feature.
macro_rules! forward {
    ($self:ident, $engine:ident => $call:expr) => {
        match $self {
            Self::Deepgram($engine) => $call,
            #[cfg(feature = "wispr")]
            Self::Wispr($engine) => $call,
        }
    };
}

impl SpeechToText for Provider {
    fn kind(&self) -> ProviderKind {
        forward!(self, engine => engine.kind())
    }

    fn label(&self) -> String {
        forward!(self, engine => engine.label())
    }

    fn encode(&self, samples: &[i16]) -> Result<EncodedAudio, SttError> {
        forward!(self, engine => engine.encode(samples))
    }

    fn request_key(&self, voices: StemVoices) -> String {
        forward!(self, engine => engine.request_key(voices))
    }

    fn request_timeout(&self, audio_bytes: usize) -> std::time::Duration {
        forward!(self, engine => engine.request_timeout(audio_bytes))
    }

    fn language_check_key(&self, voices: StemVoices) -> String {
        forward!(self, engine => engine.language_check_key(voices))
    }

    fn check_reachable(&self) -> Result<(), SttError> {
        forward!(self, engine => engine.check_reachable())
    }

    fn transcribe(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        forward!(self, engine => engine.transcribe(audio, voices))
    }

    fn language_check(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        forward!(self, engine => engine.language_check(audio, voices))
    }
}
