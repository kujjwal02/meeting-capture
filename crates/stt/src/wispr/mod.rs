//! The Wispr Flow dictation engine. It drives Wispr's own `TranscribeStream` gRPC API with a
//! token borrowed from the installed desktop app, the same method the app uses.
//!
//! This is unsupported and against Wispr's terms of service, so it is built only with the `wispr`
//! feature and offered only when the desktop app is installed.
//!
//! Wispr is a dictation transcriber: it returns plaintext per window, with no speaker labels and
//! no word timings. So each window becomes one utterance spanning the window, and the language
//! check tags words by script (Devanagari means Hindi) rather than by a provider tag.

mod auth;
mod grpc;
mod wav;
mod wire;

use std::net::{TcpStream, ToSocketAddrs as _};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ogg_opus::{EncodedAudio, STEM_RATE_HZ};
use crate::{ProviderKind, SpeechToText, StemVoices, SttError, TaggedWord, Utterance, Word};

/// The inference proxy, which authenticates with the borrowed user token alone.
const PROXY_HOST: &str = "inference.wisprflow.com";
const HTTPS_PORT: u16 = 443;
const REACHABLE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Wispr's processing and reply time, before the upload's share from its size.
const BASE_TIMEOUT: Duration = Duration::from_secs(60);
/// The slowest uplink the timeout allows for, matching the other engines.
const MIN_UPLOAD_BYTES_PER_S: u64 = 20_000;
/// A canonical PCM WAV header is this many bytes before the samples.
const WAV_HEADER_BYTES: usize = 44;

/// Whether the Wispr Flow desktop app is installed on this Mac. Only then is the engine offered.
pub(crate) fn is_installed() -> bool {
    PathBuf::from("/Applications/Wispr Flow.app").is_dir() || auth::is_signed_in()
}

#[derive(Clone)]
pub struct Wispr {
    client: reqwest::blocking::Client,
    /// The Wispr language codes for the transcript, such as `["en", "hien"]`.
    languages: Vec<String>,
    /// The language setting, for the label and the request key.
    language_label: String,
}

impl std::fmt::Debug for Wispr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Wispr")
            .field("languages", &self.languages)
            .finish_non_exhaustive()
    }
}

/// The Wispr language codes for a transcript in `language`. `multi` covers Hindi-English
/// code-switching, which is the reason to reach for Wispr at all.
fn languages_for(language: &str) -> Vec<String> {
    let owned = |codes: &[&str]| codes.iter().map(|&code| code.to_owned()).collect();
    let primary = language.split('-').next().unwrap_or(language);
    // Wispr knows only these codes; an unknown one carries no preference, so default to English
    // rather than leave the request unlabelled while the cache key still names the language.
    match primary {
        "multi" => owned(&["en", "hien"]),
        "hi" => owned(&["hien", "hi", "en"]),
        "engb" => owned(&["engb"]),
        _ => owned(&["en"]),
    }
}

/// The languages the language check asks for, so Hindi speech comes back in Devanagari to tag.
fn language_check_languages() -> Vec<String> {
    vec!["hien".to_owned(), "hi".to_owned(), "en".to_owned()]
}

impl Wispr {
    pub(crate) fn new(model: &str, language: &str) -> Result<Self, SttError> {
        // Wispr has no model to select; the setting is ignored but kept out of the request.
        let _ = model;
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(None)
            .build()
            .map_err(|error| SttError::Network {
                provider: "wispr".to_owned(),
                detail: format!("could not build the HTTP client: {error}"),
            })?;
        Ok(Self {
            client,
            languages: languages_for(language),
            language_label: language.to_owned(),
        })
    }

    fn send(
        &self,
        audio: &EncodedAudio,
        languages: &[String],
    ) -> Result<serde_json::Value, SttError> {
        let session = auth::borrow(&self.client)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let request_id = uuid::Uuid::new_v4().to_string();
        let messages = vec![
            wire::init_request(&session.user_id, &session_id, &request_id, languages),
            wire::audio_file_request(&audio.bytes),
        ];
        let text = grpc::transcribe_stream(
            PROXY_HOST,
            &session.access_token,
            &messages,
            request_timeout(audio.bytes.len()),
            &self.label(),
        )?;
        let reply = WisprReply::of(text.best_plaintext(), wav_duration_s(audio.bytes.len()));
        serde_json::to_value(reply).map_err(|error| SttError::UnreadableReply {
            provider: self.label(),
            detail: error.to_string(),
        })
    }
}

/// 60 s plus the upload at the slowest uplink allowed for.
fn request_timeout(audio_bytes: usize) -> Duration {
    let audio_bytes = u64::try_from(audio_bytes).unwrap_or(u64::MAX);
    BASE_TIMEOUT + Duration::from_secs(audio_bytes.div_ceil(MIN_UPLOAD_BYTES_PER_S))
}

/// The seconds of 16 kHz mono 16-bit audio in a WAV of `byte_len`.
#[expect(
    clippy::cast_precision_loss,
    reason = "a window holds far fewer than 2^52 samples"
)]
fn wav_duration_s(byte_len: usize) -> f64 {
    let sample_bytes = byte_len.saturating_sub(WAV_HEADER_BYTES);
    (sample_bytes / 2) as f64 / f64::from(STEM_RATE_HZ)
}

fn key_of(languages: &[String], suffix: &str) -> String {
    format!(
        "wispr?lang={}&encoding=wav-16k{suffix}",
        languages.join("+")
    )
}

/// The stored reply: Wispr returns only plaintext, so the words carry no timings and are tagged
/// by script for the language check.
#[derive(Serialize, Deserialize)]
struct WisprReply {
    plaintext: String,
    duration_s: f64,
    words: Vec<WisprWord>,
}

#[derive(Serialize, Deserialize)]
struct WisprWord {
    text: String,
    /// `hi` for a word with Devanagari, else `en`.
    language: String,
}

impl WisprReply {
    fn of(plaintext: &str, duration_s: f64) -> Self {
        let words = plaintext
            .split_whitespace()
            .map(|text| WisprWord {
                text: text.to_owned(),
                language: if has_devanagari(text) { "hi" } else { "en" }.to_owned(),
            })
            .collect();
        Self {
            plaintext: plaintext.to_owned(),
            duration_s,
            words,
        }
    }
}

/// Whether `text` holds a Devanagari character, the script Wispr returns Hindi in.
fn has_devanagari(text: &str) -> bool {
    text.chars()
        .any(|character| ('\u{0900}'..='\u{097F}').contains(&character))
}

impl SpeechToText for Wispr {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Wispr
    }

    fn label(&self) -> String {
        format!("wispr/{}", self.language_label)
    }

    fn encode(&self, samples: &[i16]) -> Result<EncodedAudio, SttError> {
        wav::encode(samples)
    }

    fn request_key(&self, _voices: StemVoices) -> String {
        key_of(&self.languages, "")
    }

    fn request_timeout(&self, audio_bytes: usize) -> Duration {
        request_timeout(audio_bytes)
    }

    fn language_check_key(&self, _voices: StemVoices) -> String {
        key_of(&language_check_languages(), "&check")
    }

    fn check_reachable(&self) -> Result<(), SttError> {
        let unreachable = |detail: String| SttError::Network {
            provider: self.label(),
            detail,
        };
        let addresses = (PROXY_HOST, HTTPS_PORT)
            .to_socket_addrs()
            .map_err(|error| unreachable(format!("could not look up {PROXY_HOST}: {error}")))?;
        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, REACHABLE_TIMEOUT) {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(unreachable(last_error.map_or_else(
            || format!("{PROXY_HOST} has no address"),
            |error| format!("could not connect to {PROXY_HOST}: {error}"),
        )))
    }

    fn transcribe(
        &self,
        audio: &EncodedAudio,
        _voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        self.send(audio, &self.languages)
    }

    fn language_check(
        &self,
        audio: &EncodedAudio,
        _voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        self.send(audio, &language_check_languages())
    }
}

fn unreadable(detail: String) -> SttError {
    SttError::UnreadableReply {
        provider: ProviderKind::Wispr.as_str().to_owned(),
        detail,
    }
}

pub(crate) fn read_utterances(reply: &serde_json::Value) -> Result<Vec<Utterance>, SttError> {
    let reply = WisprReply::deserialize(reply).map_err(|error| unreadable(error.to_string()))?;
    if reply.plaintext.trim().is_empty() {
        return Ok(Vec::new());
    }
    // One utterance spanning the window: Wispr gives no word timings to split on.
    Ok(vec![Utterance {
        start_s: 0.0,
        end_s: reply.duration_s.max(0.0),
        speaker: None,
        text: reply.plaintext,
        words: Vec::<Word>::new(),
    }])
}

pub(crate) fn read_word_languages(reply: &serde_json::Value) -> Result<Vec<TaggedWord>, SttError> {
    let reply = WisprReply::deserialize(reply).map_err(|error| unreadable(error.to_string()))?;
    Ok(reply
        .words
        .into_iter()
        .map(|word| TaggedWord {
            start_s: 0.0,
            language: word.language,
        })
        .collect())
}
