//! Speech-to-text of stretches of a 16 kHz mono stem through one `SpeechToText` trait. Deepgram is
//! the only provider today; local models join later as more variants of `Provider`.
//!
//! Audio goes out as Ogg Opus, a tenth of the WAV's size. A provider returns its reply as it
//! came, so the caller can save it and read it again later with no second request.

mod deepgram;
pub mod ogg_opus;

use serde::{Deserialize, Serialize};

pub use crate::deepgram::Deepgram;
pub use crate::ogg_opus::EncodedAudio;

/// The speech-to-text services the settings can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Deepgram's prerecorded API, with a key from the settings file.
    Deepgram,
}

impl ProviderKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deepgram => "deepgram",
        }
    }

    /// The model used when the settings name none.
    #[must_use]
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Deepgram => "nova-3",
        }
    }

    /// The language used when the settings name none. `multi` covers Hindi-English
    /// code-switching.
    #[must_use]
    pub fn default_language(self) -> &'static str {
        match self {
            Self::Deepgram => "multi",
        }
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
            | Self::RequestRejected { .. } => FailureClass::Refused,
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
}

impl Provider {
    /// The provider for `kind`, asking `model` in `language`.
    ///
    /// # Errors
    ///
    /// Returns `SttError::Network` when the HTTP client cannot be built.
    pub fn new(
        kind: ProviderKind,
        api_key: String,
        model: String,
        language: String,
    ) -> Result<Self, SttError> {
        match kind {
            ProviderKind::Deepgram => Ok(Self::Deepgram(Deepgram::new(api_key, model, language)?)),
        }
    }
}

impl SpeechToText for Provider {
    fn kind(&self) -> ProviderKind {
        match self {
            Self::Deepgram(deepgram) => deepgram.kind(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Deepgram(deepgram) => deepgram.label(),
        }
    }

    fn request_key(&self, voices: StemVoices) -> String {
        match self {
            Self::Deepgram(deepgram) => deepgram.request_key(voices),
        }
    }

    fn request_timeout(&self, audio_bytes: usize) -> std::time::Duration {
        match self {
            Self::Deepgram(deepgram) => deepgram.request_timeout(audio_bytes),
        }
    }

    fn language_check_key(&self, voices: StemVoices) -> String {
        match self {
            Self::Deepgram(deepgram) => deepgram.language_check_key(voices),
        }
    }

    fn check_reachable(&self) -> Result<(), SttError> {
        match self {
            Self::Deepgram(deepgram) => deepgram.check_reachable(),
        }
    }

    fn transcribe(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        match self {
            Self::Deepgram(deepgram) => deepgram.transcribe(audio, voices),
        }
    }

    fn language_check(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        match self {
            Self::Deepgram(deepgram) => deepgram.language_check(audio, voices),
        }
    }
}
