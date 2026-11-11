//! Deepgram's prerecorded API: one `POST /v1/listen` per Ogg Opus file.
//! <https://developers.deepgram.com/reference/speech-to-text/listen-pre-recorded>

use std::net::{TcpStream, ToSocketAddrs as _};
use std::time::Duration;

use serde::Deserialize;

use crate::{
    EncodedAudio, ProviderKind, SpeechToText, StemVoices, SttError, TaggedWord, Utterance, Word,
};

const LISTEN_URL: &str = "https://api.deepgram.com/v1/listen";
/// The host of `LISTEN_URL`, which the reachability check connects to.
const LISTEN_HOST: &str = "api.deepgram.com";
const HTTPS_PORT: u16 = 443;
/// A link that cannot open a connection in this time cannot carry an upload either.
const REACHABLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Covers Deepgram's processing and the reply. The upload's share comes on top, from the size.
const BASE_TIMEOUT: Duration = Duration::from_secs(60);
/// The slowest uplink the timeout allows for: a weak mobile link still moves this much.
const MIN_UPLOAD_BYTES_PER_S: u64 = 20_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// `diarize=true` is deprecated and routes to the v1 diarizer. v2 is the batch diarizer, pinned so
// a saved reply's request key keeps meaning the same diarizer.
const DIARIZER: &str = "v2";

/// Names the audio format in the request keys, so a change of encoding asks again.
const ENCODING_KEY: &str = "encoding=ogg-opus-24k";

/// The language the language check asks for: each word comes back tagged. The rest of its query
/// is a transcription's, so its reply also serves as the transcript of the clip.
const LANGUAGE_CHECK_LANGUAGE: &str = "multi";

#[derive(Clone)]
pub struct Deepgram {
    client: reqwest::blocking::Client,
    api_key: String,
    model: String,
    language: String,
}

// A stray `{:?}` in a log line must not print the key.
impl std::fmt::Debug for Deepgram {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Deepgram")
            .field("model", &self.model)
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl Deepgram {
    pub(crate) fn new(api_key: String, model: String, language: String) -> Result<Self, SttError> {
        // No client-wide timeout: each request sets its own from the size of its audio.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(None)
            .build()
            .map_err(|error| SttError::Network {
                provider: "deepgram".to_owned(),
                detail: format!("could not build the HTTP client: {error}"),
            })?;
        Ok(Self {
            client,
            api_key,
            model,
            language,
        })
    }

    /// The query, in a fixed order, so it doubles as the saved reply's request key.
    fn transcribe_query<'a>(
        &'a self,
        language: &'a str,
        voices: StemVoices,
    ) -> Vec<(&'static str, &'a str)> {
        let mut query = vec![
            ("model", self.model.as_str()),
            ("language", language),
            ("smart_format", "true"),
            ("utterances", "true"),
            // Calls carry client data, so Deepgram keeps none of it to train on.
            ("mip_opt_out", "true"),
        ];
        match voices {
            StemVoices::OnePerson => {}
            StemVoices::Several => query.push(("diarize_model", DIARIZER)),
        }
        query
    }

    fn send(
        &self,
        audio: &EncodedAudio,
        query: &[(&'static str, &str)],
    ) -> Result<serde_json::Value, SttError> {
        let limit = request_timeout(audio.bytes.len());
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Token {}", self.api_key)).map_err(
                |_| SttError::KeyMalformed {
                    provider: self.label(),
                },
            )?;
        // Kept out of reqwest's debug output and any HTTP/2 header table.
        authorization.set_sensitive(true);
        let response = self
            .client
            .post(LISTEN_URL)
            .query(query)
            .timeout(limit)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .header(reqwest::header::CONTENT_TYPE, "audio/ogg")
            .body(audio.bytes.clone())
            .send()
            .map_err(|error| self.request_failure(error, limit))?;
        let status = response.status();
        let body = response
            .text()
            .map_err(|error| self.request_failure(error, limit))?;
        if !status.is_success() {
            return Err(self.failure(status, &body));
        }
        serde_json::from_str(&body).map_err(|error| SttError::UnreadableReply {
            provider: self.label(),
            detail: error.to_string(),
        })
    }

    fn request_failure(&self, error: reqwest::Error, limit: Duration) -> SttError {
        if error.is_timeout() {
            SttError::Timeout {
                provider: self.label(),
                limit_s: limit.as_secs(),
            }
        } else {
            // The URL holds no key, but its query is noise in the job store.
            SttError::Network {
                provider: self.label(),
                detail: error.without_url().to_string(),
            }
        }
    }

    fn failure(&self, status: reqwest::StatusCode, body: &str) -> SttError {
        let provider = self.label();
        // Only Deepgram's own error fields: the job store keeps this text.
        let detail = serde_json::from_str::<ErrorReply>(body).map_or_else(
            |_| format!("HTTP {}", status.as_u16()),
            |reply| {
                format!(
                    "HTTP {} {}: {}",
                    status.as_u16(),
                    reply.err_code.unwrap_or_default(),
                    reply.err_msg.unwrap_or_default()
                )
            },
        );
        match status.as_u16() {
            code @ (401 | 403) => SttError::KeyRefused {
                provider,
                status: code,
            },
            402 => SttError::OutOfCredit { provider },
            429 => SttError::RateLimited { provider },
            // 504 is Deepgram giving up on its own processing time; 408 a slow upload.
            408 | 500..=599 => SttError::ServerError { provider, detail },
            _ => SttError::RequestRejected { provider, detail },
        }
    }
}

/// 60 s plus the upload at the slowest uplink allowed for.
fn request_timeout(audio_bytes: usize) -> Duration {
    let audio_bytes = u64::try_from(audio_bytes).unwrap_or(u64::MAX);
    BASE_TIMEOUT + Duration::from_secs(audio_bytes.div_ceil(MIN_UPLOAD_BYTES_PER_S))
}

fn key_of(query: &[(&'static str, &str)]) -> String {
    let query = query
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    format!("deepgram?{query}&{ENCODING_KEY}")
}

#[derive(Deserialize)]
struct ErrorReply {
    err_code: Option<String>,
    err_msg: Option<String>,
}

#[derive(Deserialize)]
struct ListenReply {
    results: ListenResults,
}

#[derive(Deserialize)]
struct ListenResults {
    utterances: Option<Vec<ListenUtterance>>,
    #[serde(default)]
    channels: Vec<ListenChannel>,
}

#[derive(Deserialize)]
struct ListenUtterance {
    start: f64,
    end: f64,
    transcript: String,
    speaker: Option<u32>,
    #[serde(default)]
    words: Vec<ListenWord>,
}

#[derive(Deserialize)]
struct ListenChannel {
    alternatives: Vec<ListenAlternative>,
}

#[derive(Deserialize)]
struct ListenAlternative {
    #[serde(default)]
    words: Vec<ListenWord>,
}

#[derive(Deserialize)]
struct ListenWord {
    word: String,
    start: f64,
    end: f64,
    punctuated_word: Option<String>,
    language: Option<String>,
}

impl SpeechToText for Deepgram {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Deepgram
    }

    fn label(&self) -> String {
        format!("deepgram/{}", self.model)
    }

    fn request_key(&self, voices: StemVoices) -> String {
        key_of(&self.transcribe_query(&self.language, voices))
    }

    fn request_timeout(&self, audio_bytes: usize) -> Duration {
        request_timeout(audio_bytes)
    }

    fn language_check_key(&self, voices: StemVoices) -> String {
        key_of(&self.transcribe_query(LANGUAGE_CHECK_LANGUAGE, voices))
    }

    fn check_reachable(&self) -> Result<(), SttError> {
        let unreachable = |detail: String| SttError::Network {
            provider: self.label(),
            detail,
        };
        let addresses = (LISTEN_HOST, HTTPS_PORT)
            .to_socket_addrs()
            .map_err(|error| unreachable(format!("could not look up {LISTEN_HOST}: {error}")))?;
        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect_timeout(&address, REACHABLE_TIMEOUT) {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(unreachable(last_error.map_or_else(
            || format!("{LISTEN_HOST} has no address"),
            |error| format!("could not connect to {LISTEN_HOST}: {error}"),
        )))
    }

    fn transcribe(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        self.send(audio, &self.transcribe_query(&self.language, voices))
    }

    fn language_check(
        &self,
        audio: &EncodedAudio,
        voices: StemVoices,
    ) -> Result<serde_json::Value, SttError> {
        self.send(
            audio,
            &self.transcribe_query(LANGUAGE_CHECK_LANGUAGE, voices),
        )
    }
}

fn unreadable(detail: String) -> SttError {
    SttError::UnreadableReply {
        provider: ProviderKind::Deepgram.as_str().to_owned(),
        detail,
    }
}

pub(crate) fn read_utterances(reply: &serde_json::Value) -> Result<Vec<Utterance>, SttError> {
    let reply = ListenReply::deserialize(reply).map_err(|error| unreadable(error.to_string()))?;
    let utterances = reply
        .results
        .utterances
        .ok_or_else(|| unreadable("no results.utterances; the request asks for them".to_owned()))?;
    Ok(utterances
        .into_iter()
        .filter(|utterance| !utterance.transcript.trim().is_empty())
        .map(|utterance| Utterance {
            start_s: utterance.start,
            end_s: utterance.end.max(utterance.start),
            speaker: utterance.speaker,
            text: utterance.transcript,
            words: utterance
                .words
                .into_iter()
                .map(|word| Word {
                    start_s: word.start,
                    end_s: word.end.max(word.start),
                    text: word.punctuated_word.unwrap_or(word.word),
                })
                .collect(),
        })
        .collect())
}

pub(crate) fn read_word_languages(reply: &serde_json::Value) -> Result<Vec<TaggedWord>, SttError> {
    let reply = ListenReply::deserialize(reply).map_err(|error| unreadable(error.to_string()))?;
    let channel = reply
        .results
        .channels
        .into_iter()
        .next()
        .ok_or_else(|| unreadable("no results.channels".to_owned()))?;
    let alternative = channel
        .alternatives
        .into_iter()
        .next()
        .ok_or_else(|| unreadable("no results.channels[0].alternatives".to_owned()))?;
    Ok(alternative
        .words
        .into_iter()
        .filter_map(|word| {
            word.language.map(|language| TaggedWord {
                start_s: word.start,
                language,
            })
        })
        .collect())
}
