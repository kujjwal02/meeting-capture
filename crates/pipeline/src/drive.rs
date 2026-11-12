//! Google Drive through the Drive account's own grant. The `drive.file` scope shows the app only
//! the files and folders it created, so a folder search never finds the user's own folders.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use md5::{Digest as _, Md5};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{CONTENT_RANGE, LOCATION, RANGE};
use reqwest::{Method, StatusCode, Url};
use serde::{Deserialize, Serialize};

use crate::google::{self, ApiErrorBody, GoogleError, OAuthClient};

const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";
const FOLDER_MEDIA_TYPE: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str = "id,name,size,md5Checksum,webViewLink,trashed";
// Drive takes every chunk but the last only in multiples of 256 KiB.
const CHUNK_BYTES: u64 = 32 * 256 * 1024;
const MAX_ATTEMPTS: u32 = 5;
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const METADATA_TIMEOUT: Duration = Duration::from_secs(30);
// reqwest's blocking client has only a whole-request timeout. A transfer gets this much plus its
// size at the slowest rate worth waiting for, so a stalled connection still ends.
const TRANSFER_BASE_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_TRANSFER_BYTES_PER_SEC: u64 = 64 * 1024;
// A saved session can have expired; a second new session that is gone at once is a Drive fault.
const MAX_SESSION_STARTS: u32 = 2;
// 308 is the resumable protocol's "send the rest", not a redirect.
const RESUME_INCOMPLETE: u16 = 308;

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    /// Clears by itself; the caller may try again later.
    #[error("Google Drive could not be reached: {message}")]
    Network { message: String },
    #[error("Google Drive account {account} needs to sign in again")]
    SignInAgain { account: String },
    #[error("the Google Drive of {account} is full")]
    QuotaExceeded { account: String },
    #[error("Google Drive has no {what}")]
    NotFound { what: String },
    /// Drive empties the trash after 30 days, so a trashed file is no copy to rely on.
    #[error("Drive file {file_id} is in the trash")]
    Trashed { file_id: FileId },
    #[error("Google Drive refused the request: {message}")]
    Other { message: String },
    #[error("could not use local file {path}: {source}")]
    LocalFile {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The copy at one end differs from the other. `actual` is "none" when Drive reports no value.
    #[error("{field} of Drive file {file_id} does not match: expected {expected}, got {actual}")]
    VerificationFailed {
        file_id: FileId,
        field: &'static str,
        expected: String,
        actual: String,
    },
}

impl DriveError {
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network { .. } => true,
            Self::SignInAgain { .. }
            | Self::QuotaExceeded { .. }
            | Self::NotFound { .. }
            | Self::Trashed { .. }
            | Self::Other { .. }
            | Self::LocalFile { .. }
            | Self::VerificationFailed { .. } => false,
        }
    }

    fn local_file(path: &Path) -> impl FnOnce(std::io::Error) -> Self + '_ {
        move |source| Self::LocalFile {
            path: path.to_owned(),
            source,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FolderId(String);

impl FolderId {
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FolderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileId(String);

impl FileId {
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FileId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A file as Drive reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveFile {
    pub id: FileId,
    pub name: String,
    pub size_bytes: Option<u64>,
    /// Lowercase hex, as `local_md5` gives it. Absent for Google Docs and other native files.
    pub md5_checksum: Option<String>,
    pub web_view_link: Option<String>,
    pub is_trashed: bool,
}

/// An upload whose size and MD5 at Drive match the local file after the upload. Only `upload`
/// makes one, and `confirm_upload` for a file a verified upload recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUpload {
    file: DriveFile,
    md5: String,
}

impl VerifiedUpload {
    #[must_use]
    pub fn file(&self) -> &DriveFile {
        &self.file
    }

    /// Lowercase hex, the same at Drive and in the local file.
    #[must_use]
    pub fn md5(&self) -> &str {
        &self.md5
    }

    #[must_use]
    pub fn into_file(self) -> DriveFile {
        self.file
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireFile {
    id: String,
    name: String,
    // Drive sends int64 fields as JSON strings.
    size: Option<String>,
    md5_checksum: Option<String>,
    web_view_link: Option<String>,
    #[serde(default)]
    trashed: bool,
}

impl TryFrom<WireFile> for DriveFile {
    type Error = DriveError;

    fn try_from(wire: WireFile) -> Result<Self, DriveError> {
        let size_bytes = wire
            .size
            .map(|size| {
                size.parse().map_err(|_| DriveError::Other {
                    message: format!("file {} has size {size:?}, not a number", wire.id),
                })
            })
            .transpose()?;
        Ok(Self {
            id: FileId(wire.id),
            name: wire.name,
            size_bytes,
            md5_checksum: wire.md5_checksum,
            web_view_link: wire.web_view_link,
            is_trashed: wire.trashed,
        })
    }
}

#[derive(Deserialize)]
struct FolderList {
    #[serde(default)]
    files: Vec<FolderEntry>,
}

#[derive(Deserialize)]
struct FolderEntry {
    id: String,
}

/// A resumable upload that Drive keeps for about a week, bound to the local file's size and
/// modification time. A caller persists it with serde. Its URI alone grants the upload, so the
/// caller stores it like a token and never logs it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSession {
    uri: String,
    local: LocalStamp,
}

/// Bytes sent to a session belong to one version of the file. Size and modification time catch
/// a rewrite without reading the whole file again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct LocalStamp {
    total_bytes: u64,
    modified_ms: u64,
}

impl LocalStamp {
    fn read(file: &File, local_path: &Path) -> Result<Self, DriveError> {
        let metadata = file
            .metadata()
            .map_err(DriveError::local_file(local_path))?;
        let modified = metadata
            .modified()
            .map_err(DriveError::local_file(local_path))?;
        let modified_ms = modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since_epoch| {
                u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX)
            });
        Ok(Self {
            total_bytes: metadata.len(),
            modified_ms,
        })
    }
}

impl std::fmt::Debug for UploadSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UploadSession")
            .finish_non_exhaustive()
    }
}

/// Drive for one signed-in account. The access token refreshes itself once on a 401.
pub struct DriveClient {
    http: Client,
    oauth_client: OAuthClient,
    account: String,
    refresh_token: String,
    access_token: Mutex<String>,
}

impl std::fmt::Debug for DriveClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DriveClient")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// What one request came back with, before retries and the account are applied.
enum Reply {
    Accepted(Response),
    Transient(String),
    Unauthorized,
    MissingScope,
    Gone,
    QuotaExceeded,
    BadRequest(String),
    Refused(String),
}

/// Whether `call` repeats a request after a network failure, a 5xx or a rate limit.
#[derive(Clone, Copy)]
enum Retries {
    Backoff,
    /// For a create: a lost reply may hide a success, and a repeat would make a duplicate.
    Never,
}

/// Where a resumable upload stands at Drive.
enum SessionState {
    Complete(DriveFile),
    Received { next_offset: u64 },
}

#[derive(Clone, Copy)]
enum SessionStep {
    AskStatus,
    SendFrom { offset: u64 },
}

enum UploadStop {
    /// The session cannot finish; a new one may.
    Restart {
        reason: String,
    },
    Failed(DriveError),
}

impl From<DriveError> for UploadStop {
    fn from(error: DriveError) -> Self {
        Self::Failed(error)
    }
}

impl DriveClient {
    /// Gets a first access token for `account`.
    ///
    /// # Errors
    ///
    /// Returns `DriveError::SignInAgain` when Google refuses the refresh token.
    pub fn connect(
        oauth_client: OAuthClient,
        account: String,
        refresh_token: String,
    ) -> Result<Self, DriveError> {
        let http = Client::builder()
            // Drive answers each call directly. A redirect would carry the bearer token.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(METADATA_TIMEOUT)
            .build()
            .map_err(|error| DriveError::Other {
                message: format!("HTTP client setup: {error}"),
            })?;
        let client = Self {
            http,
            oauth_client,
            account,
            refresh_token,
            access_token: Mutex::new(String::new()),
        };
        client.refresh_access_token()?;
        Ok(client)
    }

    #[must_use]
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Finds or creates each folder in `names` inside the one before it, the first in My Drive.
    ///
    /// # Errors
    ///
    /// Returns `DriveError` when `names` is empty or holds a blank name, or a request fails.
    pub fn ensure_folder_path(&self, names: &[&str]) -> Result<FolderId, DriveError> {
        let _span =
            tracing::info_span!("drive.ensure_folder_path", mc.drive.depth = names.len()).entered();
        let mut parent = FolderId("root".to_owned());
        if names.is_empty() {
            return Err(DriveError::Other {
                message: "a folder path needs at least one folder".to_owned(),
            });
        }
        for name in names {
            if name.trim().is_empty() {
                return Err(DriveError::Other {
                    message: "a folder name must not be blank".to_owned(),
                });
            }
            parent = match self.find_folder(name, &parent)? {
                Some(folder) => folder,
                None => self.create_folder(name, &parent)?,
            };
        }
        Ok(parent)
    }

    fn find_folder(&self, name: &str, parent: &FolderId) -> Result<Option<FolderId>, DriveError> {
        let query = format!(
            "name = '{}' and '{}' in parents and mimeType = '{FOLDER_MEDIA_TYPE}' and trashed = false",
            escape_query(name),
            escape_query(parent.as_str()),
        );
        let url = Url::parse(FILES_URL).expect("FILES_URL is a valid URL");
        let what = format!("folder {parent}");
        let response = self.call(&what, &Method::GET, &url, Retries::Backoff, |request| {
            request.query(&[
                ("q", query.as_str()),
                ("spaces", "drive"),
                ("fields", "files(id)"),
                // Two folders of one name can exist after a lost create reply or two runs at
                // once; every run settles on the oldest.
                ("orderBy", "createdTime"),
                ("pageSize", "1"),
            ])
        })?;
        let folders: FolderList = response.json().map_err(network_error)?;
        Ok(folders
            .files
            .into_iter()
            .next()
            .map(|folder| FolderId(folder.id)))
    }

    fn create_folder(&self, name: &str, parent: &FolderId) -> Result<FolderId, DriveError> {
        let url = Url::parse(FILES_URL).expect("FILES_URL is a valid URL");
        let metadata = serde_json::json!({
            "name": name,
            "mimeType": FOLDER_MEDIA_TYPE,
            "parents": [parent.as_str()],
        });
        let created = self
            .call(
                &format!("folder {parent}"),
                &Method::POST,
                &url,
                Retries::Never,
                |request| request.query(&[("fields", "id")]).json(&metadata),
            )
            .and_then(|response| response.json::<FolderEntry>().map_err(network_error));
        if let Ok(folder) = &created {
            tracing::info!(mc.drive.folder_id = %folder.id, "Drive folder created");
        }
        // Whether the create worked or its reply was lost, the oldest folder of the name wins.
        match (self.find_folder(name, parent), created) {
            (Ok(Some(oldest)), _) => Ok(oldest),
            // The listing can lag a create that just finished.
            (Ok(None) | Err(_), Ok(folder)) => Ok(FolderId(folder.id)),
            (Ok(None) | Err(_), Err(error)) => Err(error),
        }
    }

    /// Uploads `local_path` into `parent` under its file name, then checks Drive's size and MD5
    /// against the local file. With `saved_session` it goes on from the bytes Drive already
    /// holds. Each new session goes to `on_session` before any byte is sent, so the caller can
    /// store it. A saved session starts over when the file changed or Drive no longer knows it.
    ///
    /// # Errors
    ///
    /// Returns `DriveError` when the file cannot be read, Drive refuses the upload, or the copy
    /// at Drive differs from the local file.
    pub fn upload(
        &self,
        local_path: &Path,
        parent: &FolderId,
        saved_session: Option<UploadSession>,
        mut on_session: impl FnMut(&UploadSession),
    ) -> Result<VerifiedUpload, DriveError> {
        let mut file = File::open(local_path).map_err(DriveError::local_file(local_path))?;
        let local = LocalStamp::read(&file, local_path)?;
        let total_bytes = local.total_bytes;
        let _span = tracing::info_span!(
            "drive.upload",
            file.path = %local_path.display(),
            file.size = total_bytes,
        )
        .entered();
        let name = local_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| DriveError::Other {
                message: format!("{} has no file name", local_path.display()),
            })?;
        let media_type = media_type_for(local_path);

        let saved_session = saved_session.filter(|session| {
            let is_same_file = session.local == local;
            if !is_same_file {
                tracing::warn!("the file changed since its upload session started; starting over");
            }
            is_same_file
        });
        let mut session_starts = 0;
        let mut session = if let Some(session) = saved_session {
            session
        } else {
            session_starts += 1;
            let session = self.start_session(&name, parent, media_type, local)?;
            on_session(&session);
            session
        };
        loop {
            match self.send_session(&session, &mut file, local_path, total_bytes) {
                Ok(uploaded) => {
                    let verified = verify_upload(uploaded, local_path, total_bytes)?;
                    tracing::info!(mc.drive.file_id = %verified.file.id, "Drive upload finished");
                    return Ok(verified);
                }
                Err(UploadStop::Restart { reason }) if session_starts < MAX_SESSION_STARTS => {
                    tracing::warn!(exception.message = %reason, "Drive upload starting over");
                    session_starts += 1;
                    session = self.start_session(&name, parent, media_type, local)?;
                    on_session(&session);
                }
                Err(UploadStop::Restart { reason }) => {
                    return Err(DriveError::Other { message: reason });
                }
                Err(UploadStop::Failed(error)) => return Err(error),
            }
        }
    }

    fn start_session(
        &self,
        name: &str,
        parent: &FolderId,
        media_type: &str,
        local: LocalStamp,
    ) -> Result<UploadSession, DriveError> {
        let url = Url::parse(UPLOAD_URL).expect("UPLOAD_URL is a valid URL");
        let metadata = serde_json::json!({ "name": name, "parents": [parent.as_str()] });
        let response = self.call(
            &format!("folder {parent}"),
            &Method::POST,
            &url,
            // An unused session costs nothing and expires, so a lost reply is safe to repeat.
            Retries::Backoff,
            |request| {
                request
                    // Drive applies `fields` to the reply of the last chunk.
                    .query(&[("uploadType", "resumable"), ("fields", FILE_FIELDS)])
                    .header("X-Upload-Content-Type", media_type)
                    .header("X-Upload-Content-Length", local.total_bytes)
                    .json(&metadata)
            },
        )?;
        let uri = response
            .headers()
            .get(LOCATION)
            .and_then(|location| location.to_str().ok())
            .ok_or_else(|| DriveError::Other {
                message: "Drive started an upload but sent no session URI".to_owned(),
            })?;
        Ok(UploadSession {
            uri: uri.to_owned(),
            local,
        })
    }

    fn send_session(
        &self,
        session: &UploadSession,
        file: &mut File,
        local_path: &Path,
        total_bytes: u64,
    ) -> Result<DriveFile, UploadStop> {
        let mut step = SessionStep::AskStatus;
        let mut failures = 0;
        loop {
            let reply = self.put_session(session, step, file, local_path, total_bytes)?;
            let failure = match reply {
                Reply::Accepted(response) => match session_state(response)? {
                    SessionState::Complete(uploaded) => return Ok(uploaded),
                    SessionState::Received { next_offset } => match step {
                        SessionStep::SendFrom { offset } if next_offset <= offset => {
                            format!("Drive kept none of the chunk from byte {offset}")
                        }
                        SessionStep::AskStatus | SessionStep::SendFrom { .. } => {
                            // Drive holds every byte yet made no file, so this session is stuck.
                            if next_offset >= total_bytes && total_bytes > 0 {
                                return Err(UploadStop::Restart {
                                    reason: format!(
                                        "Drive holds all {total_bytes} bytes but made no file"
                                    ),
                                });
                            }
                            failures = 0;
                            step = SessionStep::SendFrom {
                                offset: next_offset,
                            };
                            continue;
                        }
                    },
                },
                Reply::Transient(message) => message,
                Reply::Gone => {
                    return Err(UploadStop::Restart {
                        reason: "Drive no longer knows the upload session".to_owned(),
                    });
                }
                // A status query that Drive cannot read means the session and the file disagree.
                Reply::BadRequest(message) => {
                    return Err(match step {
                        SessionStep::AskStatus => UploadStop::Restart { reason: message },
                        SessionStep::SendFrom { .. } => DriveError::Other { message }.into(),
                    });
                }
                Reply::Unauthorized | Reply::MissingScope => {
                    return Err(self.sign_in_again().into());
                }
                Reply::QuotaExceeded => return Err(self.quota_exceeded().into()),
                Reply::Refused(message) => return Err(DriveError::Other { message }.into()),
            };
            failures += 1;
            if failures >= MAX_ATTEMPTS {
                return Err(DriveError::Network { message: failure }.into());
            }
            tracing::warn!(exception.message = %failure, "Drive upload step failed; retrying");
            std::thread::sleep(backoff(failures));
            // A failed chunk may have landed in part, so Drive says where to go on.
            step = SessionStep::AskStatus;
        }
    }

    fn put_session(
        &self,
        session: &UploadSession,
        step: SessionStep,
        file: &mut File,
        local_path: &Path,
        total_bytes: u64,
    ) -> Result<Reply, DriveError> {
        // The session URI is the credential; no bearer token goes with it.
        let request = self.http.put(&session.uri);
        let request = match step {
            SessionStep::AskStatus => request
                .header(CONTENT_RANGE, format!("bytes */{total_bytes}"))
                .body(Vec::new()),
            SessionStep::SendFrom { .. } if total_bytes == 0 => request.body(Vec::new()),
            SessionStep::SendFrom { offset } => {
                let chunk_bytes = CHUNK_BYTES.min(total_bytes - offset);
                let chunk = read_chunk(file, local_path, offset, chunk_bytes)?;
                let last_byte = offset + chunk_bytes - 1;
                request
                    .header(
                        CONTENT_RANGE,
                        format!("bytes {offset}-{last_byte}/{total_bytes}"),
                    )
                    .timeout(transfer_timeout(chunk_bytes))
                    .body(chunk)
            }
        };
        let span = request_span(&Method::PUT, UPLOAD_URL);
        let _entered = span.enter();
        Ok(classify(request.send(), |status| {
            matches!(status, StatusCode::OK | StatusCode::CREATED)
                || status.as_u16() == RESUME_INCOMPLETE
        }))
    }

    /// Checks that the Drive file a verified upload recorded still has the size and MD5 it
    /// recorded, for a run that finishes an archive another run uploaded.
    ///
    /// # Errors
    ///
    /// Returns `DriveError::NotFound` when the app cannot see the file, `DriveError::Trashed`
    /// when it is in the trash, `DriveError::VerificationFailed` when either value differs, or
    /// another `DriveError`.
    pub fn confirm_upload(
        &self,
        id: &FileId,
        recorded_size_bytes: u64,
        recorded_md5: &str,
    ) -> Result<VerifiedUpload, DriveError> {
        let _span = tracing::info_span!("drive.confirm_upload", mc.drive.file_id = %id).entered();
        let remote = self.file(id)?;
        if remote.is_trashed {
            return Err(DriveError::Trashed {
                file_id: id.clone(),
            });
        }
        if remote.size_bytes != Some(recorded_size_bytes) {
            return Err(DriveError::VerificationFailed {
                file_id: id.clone(),
                field: "size",
                expected: recorded_size_bytes.to_string(),
                actual: remote
                    .size_bytes
                    .map_or_else(|| "none".to_owned(), |size| size.to_string()),
            });
        }
        if remote.md5_checksum.as_deref() != Some(recorded_md5) {
            return Err(DriveError::VerificationFailed {
                file_id: id.clone(),
                field: "MD5",
                expected: recorded_md5.to_owned(),
                actual: remote
                    .md5_checksum
                    .clone()
                    .unwrap_or_else(|| "none".to_owned()),
            });
        }
        Ok(VerifiedUpload {
            file: remote,
            md5: recorded_md5.to_owned(),
        })
    }

    /// The file's metadata.
    ///
    /// # Errors
    ///
    /// Returns `DriveError::NotFound` when the app cannot see the file, or another `DriveError`.
    pub fn file(&self, id: &FileId) -> Result<DriveFile, DriveError> {
        let response = self.call(
            &format!("file {id}"),
            &Method::GET,
            &file_url(id),
            Retries::Backoff,
            |request| request.query(&[("fields", FILE_FIELDS)]),
        )?;
        let wire: WireFile = response.json().map_err(network_error)?;
        DriveFile::try_from(wire)
    }

    /// Downloads the file to `dest_path`, replacing it in one rename once its size and MD5 match
    /// Drive's.
    ///
    /// # Errors
    ///
    /// Returns `DriveError::Trashed` when the file is in the trash, or another `DriveError` when
    /// the request or the local write fails, Drive reports no size or MD5, or either differs.
    pub fn download(&self, id: &FileId, dest_path: &Path) -> Result<DriveFile, DriveError> {
        let _span = tracing::info_span!(
            "drive.download",
            mc.drive.file_id = %id,
            file.path = %dest_path.display(),
        )
        .entered();
        let remote = self.file(id)?;
        if remote.is_trashed {
            return Err(DriveError::Trashed {
                file_id: id.clone(),
            });
        }
        let (Some(expected_bytes), Some(expected_md5)) =
            (remote.size_bytes, remote.md5_checksum.clone())
        else {
            return Err(DriveError::VerificationFailed {
                file_id: id.clone(),
                field: "size and MD5",
                expected: "both reported by Drive".to_owned(),
                actual: "none".to_owned(),
            });
        };
        let dest_dir = match dest_path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            Some(_) | None => Path::new("."),
        };
        let mut staged =
            tempfile::NamedTempFile::new_in(dest_dir).map_err(DriveError::local_file(dest_dir))?;
        let timeout = transfer_timeout(expected_bytes);
        let mut response = self.call(
            &format!("file {id}"),
            &Method::GET,
            &file_url(id),
            Retries::Backoff,
            |request| request.query(&[("alt", "media")]).timeout(timeout),
        )?;
        let mut writer = ChecksummedWriter {
            file: staged.as_file_mut(),
            hasher: Md5::new(),
            written_bytes: 0,
        };
        response.copy_to(&mut writer).map_err(network_error)?;
        if writer.written_bytes != expected_bytes {
            return Err(DriveError::VerificationFailed {
                file_id: id.clone(),
                field: "size",
                expected: expected_bytes.to_string(),
                actual: writer.written_bytes.to_string(),
            });
        }
        let downloaded_md5 = hex(&writer.hasher.finalize());
        if downloaded_md5 != expected_md5 {
            return Err(DriveError::VerificationFailed {
                file_id: id.clone(),
                field: "MD5",
                expected: expected_md5,
                actual: downloaded_md5,
            });
        }
        staged
            .as_file()
            .sync_all()
            .map_err(DriveError::local_file(dest_path))?;
        staged
            .persist(dest_path)
            .map_err(|error| DriveError::local_file(dest_path)(error.error))?;
        Ok(remote)
    }

    /// Sends a Drive API request with the bearer token, refreshing the token once on a 401. With
    /// `Retries::Backoff` it repeats network failures, 5xx and rate limits with backoff.
    fn call(
        &self,
        what: &str,
        method: &Method,
        url: &Url,
        retries: Retries,
        configure: impl Fn(RequestBuilder) -> RequestBuilder,
    ) -> Result<Response, DriveError> {
        let span = request_span(method, url.as_str());
        let _entered = span.enter();
        let mut has_refreshed = false;
        let mut attempt = 1;
        loop {
            let access_token = self
                .access_token
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(access_token);
            match classify(configure(request).send(), |status| status.is_success()) {
                Reply::Accepted(response) => return Ok(response),
                Reply::Unauthorized if !has_refreshed => {
                    has_refreshed = true;
                    self.refresh_access_token()?;
                }
                Reply::Unauthorized | Reply::MissingScope => return Err(self.sign_in_again()),
                Reply::Gone => {
                    return Err(DriveError::NotFound {
                        what: what.to_owned(),
                    });
                }
                Reply::QuotaExceeded => return Err(self.quota_exceeded()),
                Reply::BadRequest(message) | Reply::Refused(message) => {
                    return Err(DriveError::Other { message });
                }
                Reply::Transient(message) => {
                    let max_attempts = match retries {
                        Retries::Backoff => MAX_ATTEMPTS,
                        Retries::Never => 1,
                    };
                    if attempt >= max_attempts {
                        return Err(DriveError::Network { message });
                    }
                    tracing::warn!(exception.message = %message, "Drive request failed; retrying");
                    std::thread::sleep(backoff(attempt));
                    attempt += 1;
                }
            }
        }
    }

    fn refresh_access_token(&self) -> Result<(), DriveError> {
        let access_token =
            google::access_token(&self.oauth_client, &self.account, Some(&self.refresh_token))
                .map_err(|error| self.token_error(error))?;
        *self
            .access_token
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = access_token;
        Ok(())
    }

    fn token_error(&self, error: GoogleError) -> DriveError {
        match error {
            GoogleError::SignInAgain { .. } | GoogleError::NoClientSecret => self.sign_in_again(),
            GoogleError::TokenUnreachable(message) => DriveError::Network { message },
            GoogleError::Http { source, .. } => network_error(source),
            GoogleError::NoRefreshToken
            | GoogleError::Redirect(_)
            | GoogleError::SignInTimedOut
            | GoogleError::SignInRefused { .. }
            | GoogleError::NoCalendarAccess
            | GoogleError::NoDriveAccess
            | GoogleError::OpenBrowser(_)
            | GoogleError::TokenRequest(_)
            | GoogleError::Forbidden { .. } => DriveError::Other {
                message: error.to_string(),
            },
        }
    }

    fn sign_in_again(&self) -> DriveError {
        DriveError::SignInAgain {
            account: self.account.clone(),
        }
    }

    fn quota_exceeded(&self) -> DriveError {
        DriveError::QuotaExceeded {
            account: self.account.clone(),
        }
    }
}

/// The file's MD5 as lowercase hex, read in chunks, to compare with Drive's `md5Checksum`.
///
/// # Errors
///
/// Returns `DriveError::LocalFile` when the file cannot be read.
pub fn local_md5(path: &Path) -> Result<String, DriveError> {
    let mut file = File::open(path).map_err(DriveError::local_file(path))?;
    let mut hasher = Md5::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read_bytes = file
            .read(&mut buffer)
            .map_err(DriveError::local_file(path))?;
        if read_bytes == 0 {
            return Ok(hex(&hasher.finalize()));
        }
        hasher.update(&buffer[..read_bytes]);
    }
}

fn verify_upload(
    uploaded: DriveFile,
    local_path: &Path,
    total_bytes: u64,
) -> Result<VerifiedUpload, DriveError> {
    let mismatch =
        |field, expected: String, actual: Option<String>| DriveError::VerificationFailed {
            file_id: uploaded.id.clone(),
            field,
            expected,
            actual: actual.unwrap_or_else(|| "none".to_owned()),
        };
    if uploaded.size_bytes != Some(total_bytes) {
        return Err(mismatch(
            "size",
            total_bytes.to_string(),
            uploaded.size_bytes.map(|size| size.to_string()),
        ));
    }
    // Read after the upload, so a file rewritten during it fails here.
    let local_md5 = local_md5(local_path)?;
    if uploaded.md5_checksum.as_deref() != Some(local_md5.as_str()) {
        return Err(mismatch("MD5", local_md5, uploaded.md5_checksum.clone()));
    }
    Ok(VerifiedUpload {
        file: uploaded,
        md5: local_md5,
    })
}

struct ChecksummedWriter<'file> {
    file: &'file mut File,
    hasher: Md5,
    written_bytes: u64,
}

impl Write for ChecksummedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written_bytes = self.file.write(bytes)?;
        self.hasher.update(&bytes[..written_bytes]);
        self.written_bytes += written_bytes as u64;
        Ok(written_bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn classify(sent: reqwest::Result<Response>, is_accepted: impl Fn(StatusCode) -> bool) -> Reply {
    let response = match sent {
        Ok(response) => response,
        // The URL of a session request holds its credential.
        Err(error) => return Reply::Transient(error.without_url().to_string()),
    };
    let status = response.status();
    tracing::Span::current().record("http.response.status_code", status.as_u16());
    if is_accepted(status) {
        return Reply::Accepted(response);
    }
    match status {
        StatusCode::UNAUTHORIZED => Reply::Unauthorized,
        StatusCode::NOT_FOUND | StatusCode::GONE => Reply::Gone,
        StatusCode::TOO_MANY_REQUESTS => Reply::Transient(format!("Drive answered {status}")),
        _ if status.is_server_error() => Reply::Transient(format!("Drive answered {status}")),
        // A 403 is a full Drive, a rate limit that clears by itself, or a missing scope.
        StatusCode::FORBIDDEN => {
            let Ok(body) = response.json::<ApiErrorBody>() else {
                return Reply::Refused(format!("Drive answered {status}"));
            };
            let has_reason = |wanted: &[&str]| {
                body.error
                    .errors
                    .iter()
                    .any(|detail| wanted.contains(&detail.reason.as_str()))
            };
            if has_reason(&["storageQuotaExceeded"]) {
                Reply::QuotaExceeded
            } else if has_reason(&["userRateLimitExceeded", "rateLimitExceeded"]) {
                Reply::Transient(body.error.message)
            } else if has_reason(&["insufficientPermissions"]) {
                Reply::MissingScope
            } else {
                Reply::Refused(body.error.message)
            }
        }
        _ => {
            let message = response.json::<ApiErrorBody>().map_or_else(
                |_| format!("Drive answered {status}"),
                |body| body.error.message,
            );
            if status == StatusCode::BAD_REQUEST {
                Reply::BadRequest(message)
            } else {
                Reply::Refused(message)
            }
        }
    }
}

fn session_state(response: Response) -> Result<SessionState, DriveError> {
    if response.status().as_u16() != RESUME_INCOMPLETE {
        let wire: WireFile = response.json().map_err(network_error)?;
        return DriveFile::try_from(wire).map(SessionState::Complete);
    }
    // `Range: bytes=0-<last>` names the bytes Drive holds; no header means none.
    let next_offset = match response.headers().get(RANGE) {
        None => 0,
        Some(range) => range
            .to_str()
            .ok()
            .and_then(|range| range.strip_prefix("bytes=0-"))
            .and_then(|last_byte| last_byte.parse::<u64>().ok())
            .map(|last_byte| last_byte + 1)
            .ok_or_else(|| DriveError::Other {
                message: format!("Drive sent an unreadable upload range {range:?}"),
            })?,
    };
    Ok(SessionState::Received { next_offset })
}

fn read_chunk(
    file: &mut File,
    local_path: &Path,
    offset: u64,
    chunk_bytes: u64,
) -> Result<Vec<u8>, DriveError> {
    let mut chunk = vec![0; usize::try_from(chunk_bytes).expect("a chunk is at most 8 MiB")];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut chunk))
        .map_err(DriveError::local_file(local_path))?;
    Ok(chunk)
}

fn file_url(id: &FileId) -> Url {
    let mut url = Url::parse(FILES_URL).expect("FILES_URL is a valid URL");
    url.path_segments_mut()
        .expect("FILES_URL has a path")
        .push(id.as_str());
    url
}

// The query of a session URI is its credential, so no request logs a query.
fn request_span(method: &Method, url: &str) -> tracing::Span {
    let url_without_query = url.split('?').next().unwrap_or(url);
    tracing::debug_span!(
        "drive.request",
        http.request.method = %method,
        url.full = url_without_query,
        http.response.status_code = tracing::field::Empty,
    )
}

fn escape_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn backoff(attempt: u32) -> Duration {
    FIRST_BACKOFF * 2_u32.pow(attempt.saturating_sub(1))
}

fn transfer_timeout(bytes: u64) -> Duration {
    TRANSFER_BASE_TIMEOUT + Duration::from_secs(bytes / MIN_TRANSFER_BYTES_PER_SEC)
}

fn network_error(error: reqwest::Error) -> DriveError {
    DriveError::Network {
        // The URL of a session request holds its credential.
        message: error.without_url().to_string(),
    }
}

// Drive shows and previews a file by its media type.
fn media_type_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    match extension.as_deref() {
        Some("mp4") => "video/mp4",
        Some("flac") => "audio/flac",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("ogg" | "opus") => "audio/ogg",
        Some("json") => "application/json",
        Some("txt" | "log") => "text/plain",
        Some("md") => "text/markdown",
        Some(_) | None => "application/octet-stream",
    }
}
