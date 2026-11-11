//! Borrow and refresh the Wispr Flow desktop session token.
//!
//! The desktop app stores a Supabase session in `session.json`. The access token is a short-lived
//! JWT; the app keeps it fresh while it runs. This reads that token, and mints a new one from the
//! stored refresh token when it is near expiry.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::Deserialize;

use crate::SttError;

/// The public Supabase project of the Wispr Flow app, recovered from `app.asar`.
const SUPABASE_URL: &str = "https://dodjkfqhwrzqjwkfnthl.supabase.co";
/// The Supabase `anon` client key. Role `anon`, so it is a public client key, safe to embed, and
/// is needed only to refresh the user's own token.
const SUPABASE_ANON_KEY: &str = concat!(
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.",
    "eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6ImRvZGprZnFod3J6cWp3a2ZudGhsIiwicm9sZSI6",
    "ImFub24iLCJpYXQiOjE3MTk4ODQzMDcsImV4cCI6MjAzNTQ2MDMwN30.",
    "h6EeQ_6kqFeznH25icVUX0Szn9__kc8HoSXAsxxBWG8",
);
const SESSION_TOKEN_KEY: &str = "sb-dodjkfqhwrzqjwkfnthl-auth-token";
/// Refresh when fewer seconds than this are left, so one window's work never crosses expiry.
const MIN_SECONDS_LEFT: i64 = 120;

/// A usable Wispr session: its access token and the user it belongs to.
pub(super) struct Session {
    pub(super) access_token: String,
    pub(super) user_id: String,
}

/// The macOS path of the desktop app's session file.
fn session_file_path() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("WISPR_SESSION_FILE") {
        return Some(PathBuf::from(override_path));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Application Support/Wispr Flow")
            .join("session.json"),
    )
}

/// Whether the Wispr Flow desktop app has written a session file to borrow a token from.
pub(super) fn is_signed_in() -> bool {
    session_file_path().is_some_and(|path| path.is_file())
}

#[derive(Deserialize)]
struct StoredToken {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct JwtClaims {
    sub: String,
    #[serde(default)]
    exp: Option<i64>,
}

fn session_error(detail: impl Into<String>) -> SttError {
    SttError::WisprSession {
        detail: detail.into(),
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The claims of a JWT, read without checking the signature: the token is the app's own and is
/// verified by the server that receives it.
fn jwt_claims(token: &str) -> Result<JwtClaims, SttError> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| session_error("the session token is not a JWT"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| session_error(format!("the session token is unreadable: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| session_error(format!("the session token has no claims: {error}")))
}

/// Reads the stored token and its remaining life.
fn read_stored() -> Result<(StoredToken, i64), SttError> {
    // The path holds the OS username, which reaches logs and the saved job error, so name the
    // file without it.
    let path = session_file_path().ok_or_else(|| session_error("no HOME to find the app"))?;
    let text = std::fs::read_to_string(&path).map_err(|error| {
        session_error(format!(
            "could not read the Wispr Flow session file ({}); open the app and sign in",
            error.kind()
        ))
    })?;
    let file: serde_json::Value = serde_json::from_str(&text)
        .map_err(|_| session_error("the Wispr Flow session file is not JSON".to_owned()))?;
    let raw = file
        .get(SESSION_TOKEN_KEY)
        .ok_or_else(|| session_error("the session file holds no auth token; sign in to the app"))?;
    // The value is the token object, or a JSON string holding it.
    let stored: StoredToken = match raw {
        serde_json::Value::String(text) => serde_json::from_str(text),
        other => StoredToken::deserialize(other),
    }
    .map_err(|error| session_error(format!("the session token is malformed: {error}")))?;
    let expires_at = match stored.expires_at {
        Some(expires_at) => expires_at,
        None => jwt_claims(&stored.access_token)?.exp.unwrap_or(0),
    };
    Ok((stored, expires_at - now_unix()))
}

#[derive(Deserialize)]
struct RefreshReply {
    access_token: String,
}

/// Mints a fresh access token from the stored refresh token through Supabase.
fn refresh(client: &reqwest::blocking::Client, refresh_token: &str) -> Result<String, SttError> {
    if refresh_token.is_empty() {
        return Err(session_error(
            "the session has no refresh token; open the Wispr Flow app to refresh it",
        ));
    }
    let response = client
        .post(format!(
            "{SUPABASE_URL}/auth/v1/token?grant_type=refresh_token"
        ))
        .timeout(Duration::from_secs(20))
        .header("apikey", SUPABASE_ANON_KEY)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {SUPABASE_ANON_KEY}"),
        )
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .map_err(|error| session_error(format!("could not refresh the token: {error}")))?;
    if !response.status().is_success() {
        return Err(session_error(format!(
            "the token refresh was refused (HTTP {}); open the Wispr Flow app to sign in again",
            response.status().as_u16()
        )));
    }
    let reply: RefreshReply = response.json().map_err(|error| {
        session_error(format!("the token refresh reply is unreadable: {error}"))
    })?;
    Ok(reply.access_token)
}

/// A session with a token good for at least `MIN_SECONDS_LEFT`, refreshing when near expiry.
///
/// # Errors
///
/// Returns `SttError::WisprSession` when no signed-in session is available.
pub(super) fn borrow(client: &reqwest::blocking::Client) -> Result<Session, SttError> {
    let (stored, seconds_left) = read_stored()?;
    let access_token = if seconds_left < MIN_SECONDS_LEFT {
        refresh(client, &stored.refresh_token)?
    } else {
        stored.access_token
    };
    let user_id = jwt_claims(&access_token)?.sub;
    Ok(Session {
        access_token,
        user_id,
    })
}
