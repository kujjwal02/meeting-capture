//! Accepts TranscripTonic's webhook on 127.0.0.1 and saves the raw payload.
//!
//! The payload is written to disk before parsing, so a parser bug never loses a meeting. The
//! parsed transcript goes to the app over a channel; this crate never touches the job store.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use sources::Transcript;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// The default receiver port, set in TranscripTonic as `http://127.0.0.1:47615/transcriptonic`.
pub const DEFAULT_PORT: u16 = 47615;

pub const WEBHOOK_PATH: &str = "/transcriptonic";

/// The Chrome Web Store build of TranscripTonic. Its background fetch sends this origin.
const TRANSCRIPTONIC_ORIGIN: &str = "chrome-extension://ciepnfnceimjehngolkijpnbappkkiag";

// A four-hour Meet call is about 800 KB of advanced body.
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// A parsed transcript and the raw payload file it came from.
#[derive(Debug)]
pub struct ReceivedCaptions {
    pub payload_path: PathBuf,
    pub transcript: Transcript,
}

struct ReceiverState {
    payload_dir: PathBuf,
    allowed_hosts: [String; 2],
    received: mpsc::Sender<ReceivedCaptions>,
}

/// Serves the webhook on `listener` until the future is dropped.
///
/// The caller binds the listener, so it can report a taken port before serving.
///
/// # Errors
///
/// Returns the I/O error that stopped the server.
pub async fn serve(
    listener: TcpListener,
    payload_dir: PathBuf,
    received: mpsc::Sender<ReceivedCaptions>,
) -> std::io::Result<()> {
    let port = listener.local_addr()?.port();
    let state = Arc::new(ReceiverState {
        payload_dir,
        allowed_hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
        received,
    });
    let router = Router::new()
        .route(WEBHOOK_PATH, post(receive_transcriptonic))
        .layer(DefaultBodyLimit::max(MAX_PAYLOAD_BYTES))
        .with_state(state);
    axum::serve(listener, router).await
}

// A non-2xx reply makes TranscripTonic mark the post failed and show a retry notification.
async fn receive_transcriptonic(
    State(state): State<Arc<ReceiverState>>,
    headers: HeaderMap,
    payload: Bytes,
) -> StatusCode {
    if let Err(rejection) = check_sender(&headers, &state.allowed_hosts) {
        tracing::warn!(
            http.response.status_code = StatusCode::FORBIDDEN.as_u16(),
            rejection,
            "rejected webhook request"
        );
        return StatusCode::FORBIDDEN;
    }

    let payload_dir = state.payload_dir.clone();
    let payload_size = payload.len();
    let saved = tokio::task::spawn_blocking(move || {
        sources::transcriptonic::save_payload(&payload_dir, &payload).map(|path| (path, payload))
    })
    .await
    .unwrap_or_else(|join_error| Err(std::io::Error::other(join_error)));
    let (payload_path, payload) = match saved {
        Ok(saved) => saved,
        Err(error) => {
            tracing::error!(
                exception.message = %error,
                "could not save TranscripTonic payload"
            );
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };
    tracing::info!(
        file.path = %payload_path.display(),
        http.request.body.size = payload_size,
        "saved TranscripTonic payload"
    );

    let transcript = match sources::transcriptonic::parse(&payload) {
        Ok(transcript) => transcript,
        Err(error) => {
            tracing::error!(
                file.path = %payload_path.display(),
                exception.message = %error,
                "could not parse TranscripTonic payload"
            );
            return StatusCode::UNPROCESSABLE_ENTITY;
        }
    };

    let captions = ReceivedCaptions {
        payload_path,
        transcript,
    };
    if let Err(unsent) = state.received.send(captions).await {
        tracing::error!(
            file.path = %unsent.0.payload_path.display(),
            "app stopped taking transcripts; payload kept on disk"
        );
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    StatusCode::OK
}

// Any web page in the user's browser can POST to 127.0.0.1. Pages always send their own Origin,
// which they cannot forge, so only the extension's origin passes; local tools send none. A page
// can send `application/json` only after a CORS preflight, which this server never answers. The
// Host check stops DNS rebinding.
fn check_sender(headers: &HeaderMap, allowed_hosts: &[String; 2]) -> Result<(), &'static str> {
    let header_text = |name| headers.get(name).and_then(|value| value.to_str().ok());
    if header_text(header::ORIGIN).is_some_and(|origin| origin != TRANSCRIPTONIC_ORIGIN) {
        return Err("origin is not TranscripTonic");
    }
    if !header_text(header::HOST)
        .is_some_and(|host| allowed_hosts.iter().any(|allowed| allowed == host))
    {
        return Err("host is not this receiver");
    }
    let is_json = header_text(header::CONTENT_TYPE)
        .is_some_and(|content_type| content_type.starts_with("application/json"));
    if !is_json {
        return Err("content type is not application/json");
    }
    Ok(())
}
