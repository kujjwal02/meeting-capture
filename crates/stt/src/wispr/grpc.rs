//! One `TranscribeStream` call over gRPC: HTTP/2 with manual length-prefixed framing, so no
//! `tonic` or `prost` is pulled in. The audio and reply are small enough to hold in memory.

use std::time::Duration;

use bytes::{Buf as _, Bytes, BytesMut};
use http::{HeaderValue, Method, Request};

use super::wire::{ResponseText, parse_response};
use crate::SttError;

const HTTPS_PORT: u16 = 443;
const GRPC_METHOD_PATH: &str = "/flow_api.v1.TranscriptionService/TranscribeStream";
const USER_AGENT: &str = "meeting-capture grpc";

/// Sends the request messages and returns the transcription text of the reply.
///
/// `access_token` authenticates the call; `host` is the inference proxy. Each message in
/// `messages` is one protobuf `Request`, framed here as gRPC expects.
pub(super) fn transcribe_stream(
    host: &str,
    access_token: &str,
    messages: &[Vec<u8>],
    timeout: Duration,
    provider_label: &str,
) -> Result<ResponseText, SttError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| network(provider_label, format!("no runtime: {error}")))?;
    let call = call(host, access_token, messages, provider_label);
    match runtime.block_on(async { tokio::time::timeout(timeout, call).await }) {
        Ok(result) => result,
        Err(_elapsed) => Err(SttError::Timeout {
            provider: provider_label.to_owned(),
            limit_s: timeout.as_secs(),
        }),
    }
}

fn network(provider_label: &str, detail: impl Into<String>) -> SttError {
    SttError::Network {
        provider: provider_label.to_owned(),
        detail: detail.into(),
    }
}

/// One gRPC message, length-prefixed: a zero compression flag and a big-endian length.
fn frame(message: &[u8]) -> Vec<u8> {
    let length = u32::try_from(message.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(message.len() + 5);
    out.push(0);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(message);
    out
}

async fn connect(
    host: &str,
    provider_label: &str,
) -> Result<tokio_native_tls::TlsStream<tokio::net::TcpStream>, SttError> {
    let tcp = tokio::net::TcpStream::connect((host, HTTPS_PORT))
        .await
        .map_err(|error| {
            network(
                provider_label,
                format!("could not connect to {host}: {error}"),
            )
        })?;
    tcp.set_nodelay(true).ok();
    let connector = native_tls::TlsConnector::builder()
        .request_alpns(&["h2"])
        .build()
        .map_err(|error| network(provider_label, format!("TLS setup failed: {error}")))?;
    tokio_native_tls::TlsConnector::from(connector)
        .connect(host, tcp)
        .await
        .map_err(|error| network(provider_label, format!("TLS handshake failed: {error}")))
}

async fn call(
    host: &str,
    access_token: &str,
    messages: &[Vec<u8>],
    provider_label: &str,
) -> Result<ResponseText, SttError> {
    let tls = connect(host, provider_label).await?;
    let (handshake, connection) = h2::client::handshake(tls)
        .await
        .map_err(|error| network(provider_label, format!("HTTP/2 handshake failed: {error}")))?;
    // Drive the connection in the background; it ends when the streams close.
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut client = handshake
        .ready()
        .await
        .map_err(|error| network(provider_label, format!("HTTP/2 not ready: {error}")))?;

    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {access_token}")).map_err(|_| {
            SttError::KeyMalformed {
                provider: provider_label.to_owned(),
            }
        })?;
    authorization.set_sensitive(true);
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("https://{host}{GRPC_METHOD_PATH}"))
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .header(http::header::TE, "trailers")
        .header(http::header::USER_AGENT, USER_AGENT)
        .header(http::header::AUTHORIZATION, authorization)
        .body(())
        .map_err(|error| network(provider_label, format!("bad request: {error}")))?;

    let (response, mut send) = client.send_request(request, false).map_err(|error| {
        network(
            provider_label,
            format!("could not open the stream: {error}"),
        )
    })?;
    for (index, message) in messages.iter().enumerate() {
        let is_last = index + 1 == messages.len();
        send.send_data(Bytes::from(frame(message)), is_last)
            .map_err(|error| network(provider_label, format!("could not send audio: {error}")))?;
    }

    let response = response
        .await
        .map_err(|error| grpc_error(provider_label, &error))?;
    let status = response.status();
    if status != http::StatusCode::OK {
        return Err(http_status_failure(provider_label, status.as_u16()));
    }
    // A trailers-only error puts grpc-status in the response headers, with no body.
    if let Some(code) = grpc_status(Some(response.headers())) {
        return Err(grpc_status_failure(provider_label, code));
    }
    let mut body = response.into_body();
    let mut buffered = BytesMut::new();
    let mut text = ResponseText::default();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|error| grpc_error(provider_label, &error))?;
        let len = chunk.len();
        buffered.extend_from_slice(&chunk);
        drain_messages(&mut buffered, &mut text);
        let _ = body.flow_control().release_capacity(len);
    }
    let trailers = body
        .trailers()
        .await
        .map_err(|error| grpc_error(provider_label, &error))?;
    if let Some(status) = grpc_status(trailers.as_ref()) {
        return Err(grpc_status_failure(provider_label, status));
    }
    Ok(text)
}

/// Parses every whole gRPC message at the front of `buffered`, leaving a partial one.
fn drain_messages(buffered: &mut BytesMut, text: &mut ResponseText) {
    while buffered.len() >= 5 {
        let length =
            u32::from_be_bytes([buffered[1], buffered[2], buffered[3], buffered[4]]) as usize;
        let total = 5 + length;
        if buffered.len() < total {
            break;
        }
        let compressed = buffered[0] != 0;
        buffered.advance(5);
        let message = buffered.split_to(length);
        // The server sends no compressed frames; skip any it does rather than misread them.
        if !compressed {
            parse_response(&message, text);
        }
    }
}

/// The numeric `grpc-status` in a trailer map, if present and non-zero.
fn grpc_status(trailers: Option<&http::HeaderMap>) -> Option<u16> {
    let status = trailers?.get("grpc-status")?.to_str().ok()?.parse().ok()?;
    (status != 0).then_some(status)
}

fn http_status_failure(provider_label: &str, status: u16) -> SttError {
    let provider = provider_label.to_owned();
    match status {
        401 | 403 => SttError::WisprSession {
            detail: format!(
                "Wispr refused the borrowed token (HTTP {status}); open the Wispr Flow app and sign in again"
            ),
        },
        402 => SttError::OutOfCredit { provider },
        429 => SttError::RateLimited { provider },
        408 | 500..=599 => SttError::ServerError {
            provider,
            detail: format!("HTTP {status}"),
        },
        other => SttError::RequestRejected {
            provider,
            detail: format!("HTTP {other}"),
        },
    }
}

/// Maps a gRPC status code to the matching failure. Codes follow the gRPC spec.
fn grpc_status_failure(provider_label: &str, code: u16) -> SttError {
    let provider = provider_label.to_owned();
    match code {
        7 | 16 => SttError::WisprSession {
            detail: format!(
                "Wispr refused the borrowed token (gRPC {code}); open the Wispr Flow app and sign in again"
            ),
        },
        8 => SttError::RateLimited { provider },
        // Permanent: retrying the same request fails the same way. 3 INVALID_ARGUMENT,
        // 5 NOT_FOUND, 9 FAILED_PRECONDITION, 12 UNIMPLEMENTED.
        3 | 5 | 9 | 12 => SttError::RequestRejected {
            provider,
            detail: format!("gRPC {code}"),
        },
        // Transient: 13 INTERNAL, 14 UNAVAILABLE and the rest are worth another try later.
        _ => SttError::ServerError {
            provider,
            detail: format!("gRPC {code}"),
        },
    }
}

/// An h2 stream error that carries a gRPC status maps by that status; otherwise it is a network
/// fault worth another try.
fn grpc_error(provider_label: &str, error: &h2::Error) -> SttError {
    if let Some(reason) = error.reason()
        && reason == h2::Reason::REFUSED_STREAM
    {
        return SttError::ServerError {
            provider: provider_label.to_owned(),
            detail: "the stream was refused".to_owned(),
        };
    }
    network(provider_label, error.to_string())
}
