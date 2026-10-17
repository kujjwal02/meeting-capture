//! Google sign-in and the Calendar API. The client ID and the account list live in the settings
//! file; the client secret and one refresh token per account live in the OS keychain.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use oauth2::basic::{BasicClient, BasicErrorResponseType};
use oauth2::url::Url;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    PkceCodeChallenge, RedirectUrl, RefreshToken, RequestTokenError, Scope, TokenResponse as _,
    TokenUrl,
};
use serde::Deserialize;

pub use crate::keychain::{Keychain, KeychainError};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const EVENTS_URL: &str = "https://www.googleapis.com/calendar/v3/calendars/primary/events";
const CALENDAR_SCOPE: &str = "https://www.googleapis.com/auth/calendar.events.readonly";
const SCOPES: [&str; 3] = [CALENDAR_SCOPE, "openid", "email"];
const CLIENT_SECRET_USER: &str = "google-client-secret";
const SIGN_IN_TIMEOUT: Duration = Duration::from_mins(5);
// Long enough for a browser on the same machine to send its request line and headers.
const REDIRECT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
// A call fits in one page many times over.
const MAX_EVENTS: &str = "250";

#[derive(Debug, thiserror::Error)]
pub enum GoogleError {
    #[error(transparent)]
    Keychain(#[from] KeychainError),
    #[error("Google account {email} needs to sign in again")]
    SignInAgain { email: String },
    #[error("the Google client secret is not set; add it in Settings")]
    NoClientSecret,
    #[error(
        "Google sent no refresh token; remove Meeting Capture at myaccount.google.com/connections and add the account again"
    )]
    NoRefreshToken,
    #[error("could not wait for Google's sign-in redirect: {0}")]
    Redirect(std::io::Error),
    #[error("sign-in was not finished within 5 minutes")]
    SignInTimedOut,
    #[error("Google sign-in was refused: {reason}")]
    SignInRefused { reason: String },
    #[error(
        "Google did not grant calendar access; add the account again and tick the calendar box"
    )]
    NoCalendarAccess,
    #[error("could not open the browser: {0}")]
    OpenBrowser(String),
    #[error("Google token request failed: {0}")]
    TokenRequest(String),
    #[error("Google Calendar refused the request: {message}")]
    Forbidden { message: String },
    #[error("Google {action} request failed: {source}")]
    Http {
        action: &'static str,
        source: reqwest::Error,
    },
}

/// The "Desktop app" OAuth client from Google Cloud Console.
#[derive(Clone)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ClientFileError {
    #[error("the file is not JSON: {0}")]
    NotJson(String),
    #[error(
        "the file holds a {kind:?} client; create an OAuth client of type \"Desktop app\" instead"
    )]
    NotDesktopClient { kind: String },
}

#[derive(Deserialize)]
struct ClientFile {
    installed: Option<ClientFileEntry>,
    web: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ClientFileEntry {
    client_id: String,
    client_secret: String,
}

impl OAuthClient {
    /// Reads the `client_secret_*.json` file that Google Cloud Console downloads.
    ///
    /// # Errors
    ///
    /// Returns `ClientFileError` when the file is not a Desktop app client.
    pub fn from_client_file(json: &str) -> Result<Self, ClientFileError> {
        let file: ClientFile = serde_json::from_str(json)
            .map_err(|error| ClientFileError::NotJson(error.to_string()))?;
        match (file.installed, file.web) {
            (Some(entry), _) => Ok(Self {
                client_id: entry.client_id,
                client_secret: entry.client_secret,
            }),
            (None, Some(_)) => Err(ClientFileError::NotDesktopClient {
                kind: "Web application".to_owned(),
            }),
            (None, None) => Err(ClientFileError::NotDesktopClient {
                kind: "unknown".to_owned(),
            }),
        }
    }

    fn oauth(
        &self,
        redirect_url: Option<RedirectUrl>,
    ) -> BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet> {
        let client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_client_secret(ClientSecret::new(self.client_secret.clone()))
            .set_auth_uri(AuthUrl::new(AUTH_URL.to_owned()).expect("AUTH_URL is a valid URL"))
            .set_token_uri(TokenUrl::new(TOKEN_URL.to_owned()).expect("TOKEN_URL is a valid URL"));
        match redirect_url {
            Some(redirect_url) => client.set_redirect_uri(redirect_url),
            None => client,
        }
    }
}

impl Keychain {
    /// # Errors
    ///
    /// Returns `GoogleError::Keychain` when the keychain refuses the read.
    pub fn has_client_secret(&self) -> Result<bool, GoogleError> {
        Ok(self.read(CLIENT_SECRET_USER)?.is_some())
    }

    /// # Errors
    ///
    /// Returns `GoogleError::Keychain` when the keychain refuses the write.
    pub fn set_client_secret(&self, client_secret: &str) -> Result<(), GoogleError> {
        Ok(self.write(CLIENT_SECRET_USER, client_secret)?)
    }

    /// The OAuth client for `client_id`, with its secret from the keychain.
    ///
    /// # Errors
    ///
    /// Returns `GoogleError::NoClientSecret` when no secret is stored.
    pub fn oauth_client(&self, client_id: &str) -> Result<OAuthClient, GoogleError> {
        let client_secret = self
            .read(CLIENT_SECRET_USER)?
            .ok_or(GoogleError::NoClientSecret)?;
        Ok(OAuthClient {
            client_id: client_id.to_owned(),
            client_secret,
        })
    }

    /// Revokes the account's grant at Google, then deletes its refresh token. A failed revoke is
    /// logged only: the token is deleted either way, and the user can still remove the app at
    /// myaccount.google.com/connections.
    ///
    /// # Errors
    ///
    /// Returns `GoogleError::Keychain` when the keychain refuses the delete.
    pub fn forget_account(&self, email: &str) -> Result<(), GoogleError> {
        let user = refresh_token_user(email);
        if let Some(refresh_token) = self.read(&user)? {
            let revoked = http_client().and_then(|http| {
                http.post(REVOKE_URL)
                    .form(&[("token", refresh_token.as_str())])
                    .send()
                    .and_then(reqwest::blocking::Response::error_for_status)
                    .map_err(|source| GoogleError::Http {
                        action: "revoke",
                        source,
                    })
            });
            if let Err(error) = revoked {
                tracing::warn!(user.email = email, exception.message = %error, "could not revoke the Google grant");
            }
        }
        Ok(self.delete(&user)?)
    }
}

fn refresh_token_user(email: &str) -> String {
    format!("google-refresh-token:{email}")
}

fn http_client() -> Result<reqwest::blocking::Client, GoogleError> {
    reqwest::blocking::Client::builder()
        // The token endpoint must not redirect a code or a refresh token anywhere else.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|source| GoogleError::Http {
            action: "client setup",
            source,
        })
}

/// Signs an account in through the default browser: authorization code with PKCE and a loopback
/// redirect. Stores the refresh token and returns the account's email. Blocks for up to 5 minutes.
///
/// # Errors
///
/// Returns `GoogleError` when the user refuses, the time runs out, or a request fails.
pub fn sign_in(
    keychain: &Keychain,
    client: &OAuthClient,
    open_browser: impl FnOnce(&str) -> Result<(), String>,
) -> Result<String, GoogleError> {
    let _span = tracing::info_span!("google.sign_in").entered();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(GoogleError::Redirect)?;
    let port = listener.local_addr().map_err(GoogleError::Redirect)?.port();
    let redirect_url = RedirectUrl::new(format!("http://127.0.0.1:{port}"))
        .map_err(|error| GoogleError::Redirect(std::io::Error::other(error)))?;
    let oauth = client.oauth(Some(redirect_url));

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, state) = oauth
        .authorize_url(CsrfToken::new_random)
        .add_scopes(SCOPES.map(|scope| Scope::new(scope.to_owned())))
        .add_extra_param("access_type", "offline")
        // Consent each time, so Google sends a refresh token even for an account it has seen.
        .add_extra_param("prompt", "select_account consent")
        .set_pkce_challenge(pkce_challenge)
        .url();
    open_browser(auth_url.as_str()).map_err(GoogleError::OpenBrowser)?;
    let code = await_redirect(&listener, state.secret())?;

    let http = http_client()?;
    let token = oauth
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request(&http)
        .map_err(|error| GoogleError::TokenRequest(token_error_message(&error)))?;
    // Google's consent screen lets the user untick the calendar box and still sign in.
    let has_calendar_scope = token
        .scopes()
        .is_some_and(|scopes| scopes.iter().any(|scope| scope.as_str() == CALENDAR_SCOPE));
    if !has_calendar_scope {
        return Err(GoogleError::NoCalendarAccess);
    }
    let refresh_token = token
        .refresh_token()
        .ok_or(GoogleError::NoRefreshToken)?
        .secret()
        .clone();
    let email = account_email(&http, token.access_token().secret())?;
    keychain.write(&refresh_token_user(&email), &refresh_token)?;
    tracing::info!(user.email = %email, "Google account signed in");
    Ok(email)
}

#[derive(Deserialize)]
struct UserInfo {
    email: String,
}

fn account_email(
    http: &reqwest::blocking::Client,
    access_token: &str,
) -> Result<String, GoogleError> {
    let http_error = |source| GoogleError::Http {
        action: "userinfo",
        source,
    };
    let user_info: UserInfo = http
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(reqwest::blocking::Response::json)
        .map_err(http_error)?;
    Ok(user_info.email)
}

// Serves the loopback redirect. Other requests, such as the browser's favicon fetch or a port
// scan, get an error reply and the wait goes on.
fn await_redirect(listener: &TcpListener, expected_state: &str) -> Result<String, GoogleError> {
    listener
        .set_nonblocking(true)
        .map_err(GoogleError::Redirect)?;
    let deadline = Instant::now() + SIGN_IN_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            return Err(GoogleError::SignInTimedOut);
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => return Err(GoogleError::Redirect(error)),
        };
        match answer_redirect(stream, expected_state) {
            RedirectAnswer::Finished(outcome) => return outcome,
            RedirectAnswer::NotTheRedirect => {}
        }
    }
}

enum RedirectAnswer {
    Finished(Result<String, GoogleError>),
    NotTheRedirect,
}

fn answer_redirect(mut stream: TcpStream, expected_state: &str) -> RedirectAnswer {
    let answer = match read_redirect(&mut stream) {
        Ok(Some(redirect)) if redirect.state.as_deref() == Some(expected_state) => {
            RedirectAnswer::Finished(redirect.into_code())
        }
        Ok(Some(_)) => {
            tracing::warn!("a sign-in redirect with the wrong state was ignored");
            RedirectAnswer::NotTheRedirect
        }
        Ok(None) => RedirectAnswer::NotTheRedirect,
        Err(error) => {
            tracing::warn!(exception.message = %error, "unreadable request on the sign-in port");
            RedirectAnswer::NotTheRedirect
        }
    };
    let (status, page) = match &answer {
        RedirectAnswer::NotTheRedirect => ("404 Not Found", "Not found."),
        RedirectAnswer::Finished(Ok(_)) => (
            "200 OK",
            "Signed in to Meeting Capture. You can close this tab.",
        ),
        RedirectAnswer::Finished(Err(_)) => (
            "400 Bad Request",
            "Sign-in failed. Go back to Meeting Capture for the reason.",
        ),
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    // Shutting down the write half before the drop lets the browser read the page instead of a
    // connection reset.
    let sent = stream
        .write_all(reply.as_bytes())
        .and_then(|()| stream.shutdown(std::net::Shutdown::Write));
    if let Err(error) = sent {
        tracing::warn!(exception.message = %error, "could not answer the browser");
    }
    answer
}

struct Redirect {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl Redirect {
    fn into_code(self) -> Result<String, GoogleError> {
        match (self.code, self.error) {
            (_, Some(reason)) => Err(GoogleError::SignInRefused { reason }),
            (Some(code), None) => Ok(code),
            (None, None) => Err(GoogleError::SignInRefused {
                reason: "the redirect carried no code".to_owned(),
            }),
        }
    }
}

// Reads one HTTP request up to the end of its headers, within `REDIRECT_READ_TIMEOUT` in total.
// `None` means it is not the redirect.
fn read_redirect(stream: &mut TcpStream) -> std::io::Result<Option<Redirect>> {
    const MAX_REQUEST_BYTES: usize = 16 * 1024;
    stream.set_nonblocking(false)?;
    let read_deadline = Instant::now() + REDIRECT_READ_TIMEOUT;
    let mut request = Vec::new();
    let mut chunk = [0; 2048];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let remaining = read_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || request.len() >= MAX_REQUEST_BYTES {
            return Ok(None);
        }
        stream.set_read_timeout(Some(remaining))?;
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
    }
    let request = String::from_utf8_lossy(&request);
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.split(' ').next())
    else {
        return Ok(None);
    };
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Ok(None);
    };
    if url.path() != "/" {
        return Ok(None);
    }
    let query_value = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let redirect = Redirect {
        code: query_value("code"),
        state: query_value("state"),
        error: query_value("error"),
    };
    if redirect.code.is_none() && redirect.error.is_none() {
        return Ok(None);
    }
    Ok(Some(redirect))
}

fn token_error_message<E: std::error::Error>(
    error: &RequestTokenError<E, oauth2::basic::BasicErrorResponse>,
) -> String {
    match error {
        RequestTokenError::ServerResponse(response) => response.to_string(),
        RequestTokenError::Request(error) => error.to_string(),
        RequestTokenError::Parse(error, _) => error.to_string(),
        RequestTokenError::Other(message) => message.clone(),
    }
}

/// A fresh access token for `email`. A revoked or expired refresh token gives
/// `GoogleError::SignInAgain`; it never opens a browser.
///
/// # Errors
///
/// Returns `GoogleError` when the account must sign in again or the request fails.
pub fn access_token(
    keychain: &Keychain,
    client: &OAuthClient,
    email: &str,
) -> Result<String, GoogleError> {
    let sign_in_again = || GoogleError::SignInAgain {
        email: email.to_owned(),
    };
    let refresh_token = keychain
        .read(&refresh_token_user(email))?
        .ok_or_else(sign_in_again)?;
    let token = client
        .oauth(None)
        .exchange_refresh_token(&RefreshToken::new(refresh_token))
        .request(&http_client()?);
    match token {
        Ok(token) => Ok(token.access_token().secret().clone()),
        // A revoked grant, or a client ID or secret that changed since the account signed in.
        // Neither clears by itself, so retrying would fail every run.
        Err(RequestTokenError::ServerResponse(response))
            if matches!(
                response.error(),
                BasicErrorResponseType::InvalidGrant
                    | BasicErrorResponseType::InvalidClient
                    | BasicErrorResponseType::UnauthorizedClient
            ) =>
        {
            tracing::warn!(user.email = email, exception.message = %response, "Google refused the refresh token");
            Err(sign_in_again())
        }
        Err(error) => Err(GoogleError::TokenRequest(token_error_message(&error))),
    }
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<ApiErrorDetail>,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Deserialize)]
struct EventList {
    #[serde(default)]
    items: Vec<ApiEvent>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiEvent {
    pub id: String,
    pub summary: Option<String>,
    pub start: EventTime,
    pub end: EventTime,
    #[serde(default)]
    pub attendees: Vec<ApiAttendee>,
    pub hangout_link: Option<String>,
    pub conference_data: Option<ConferenceData>,
}

/// `date_time` is absent for all-day events, which carry only a date.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EventTime {
    pub date_time: Option<Timestamp>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiAttendee {
    pub email: Option<String>,
    pub display_name: Option<String>,
    #[serde(rename = "self", default)]
    pub is_self: bool,
    /// Meeting rooms and other resources.
    #[serde(default)]
    pub resource: bool,
    pub response_status: Option<ResponseStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ResponseStatus {
    NeedsAction,
    Declined,
    Tentative,
    Accepted,
    /// A status Google adds later must not fail the whole event list.
    #[serde(other)]
    Unrecognised,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConferenceData {
    #[serde(default)]
    pub entry_points: Vec<EntryPoint>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EntryPoint {
    pub uri: Option<String>,
}

/// Events on the account's primary calendar that overlap `from..to`, recurring ones expanded.
pub(crate) fn overlapping_events(
    access_token: &str,
    email: &str,
    from: Timestamp,
    to: Timestamp,
) -> Result<Vec<ApiEvent>, GoogleError> {
    let http_error = |source| GoogleError::Http {
        action: "calendar events",
        source,
    };
    let response = http_client()?
        .get(EVENTS_URL)
        .bearer_auth(access_token)
        .query(&[
            ("timeMin", from.to_string().as_str()),
            ("timeMax", to.to_string().as_str()),
            ("singleEvents", "true"),
            // Leaves out focus time, out of office and working location.
            ("eventTypes", "default"),
            ("maxResults", MAX_EVENTS),
        ])
        .send()
        .map_err(http_error)?;
    let sign_in_again = || GoogleError::SignInAgain {
        email: email.to_owned(),
    };
    match response.status() {
        reqwest::StatusCode::UNAUTHORIZED => return Err(sign_in_again()),
        // A 403 is also a rate limit, which clears by itself; only a missing scope needs sign-in.
        reqwest::StatusCode::FORBIDDEN => {
            let body: ApiErrorBody = response.json().map_err(http_error)?;
            let is_missing_scope = body
                .error
                .errors
                .iter()
                .any(|detail| detail.reason == "insufficientPermissions");
            return Err(if is_missing_scope {
                sign_in_again()
            } else {
                GoogleError::Forbidden {
                    message: body.error.message,
                }
            });
        }
        _ => {}
    }
    let events: EventList = response
        .error_for_status()
        .and_then(reqwest::blocking::Response::json)
        .map_err(http_error)?;
    Ok(events.items)
}
