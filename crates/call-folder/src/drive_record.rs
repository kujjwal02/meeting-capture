//! `drive.json`: what of a call folder's media is on Google Drive, and whether the local copy is
//! gone. The archive writes it; finalize, repair, the purge and the fragment cleanup read it and
//! leave a folder alone once every upload is recorded.
//!
//! The record moves through closed states: `uploading` while uploads land, each recorded as it
//! is verified; `uploaded` once every planned file is on Drive, written before anything is
//! deleted; `media_deleted` once the local copies are gone. A run cut short in `uploading`
//! confirms or resumes its uploads; one cut short in `uploaded` finishes the deletion.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub const DRIVE_RECORD_FILE_NAME: &str = "drive.json";

/// Raised when the layout changes in a way an older reader would misread.
pub const DRIVE_RECORD_VERSION: u32 = 1;

/// The archive's record. `Session` is the upload session type of the Drive client, which this
/// crate does not know; readers that only check the state read it as `serde::de::IgnoredAny`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveRecord<Session> {
    pub version: RecordVersion,
    /// The Drive account that holds the files.
    pub account: String,
    pub folder_id: String,
    /// From the Drive root folder down to the call's own folder.
    pub folder_path: Vec<String>,
    pub state: ArchiveState,
    pub files: Vec<ArchivedFile>,
    /// Stems that `call.mp4` holds in its mix. They are deleted with it and have no upload of
    /// their own, so no restore brings them back.
    pub mixed_stem_names: Vec<String>,
    /// Keyed by the Drive name. Each session URI grants its upload, so it is never logged.
    pub pending_sessions: BTreeMap<String, Session>,
    /// When `mc restore` last brought the media back.
    pub restored_at: Option<Timestamp>,
}

/// `version` in `drive.json`. Only the version this build writes reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct RecordVersion;

#[derive(Debug, thiserror::Error)]
#[error("drive.json field `version` is {0}; this build reads only version {DRIVE_RECORD_VERSION}")]
pub struct UnknownRecordVersion(u32);

impl TryFrom<u32> for RecordVersion {
    type Error = UnknownRecordVersion;

    fn try_from(version: u32) -> Result<Self, UnknownRecordVersion> {
        if version == DRIVE_RECORD_VERSION {
            Ok(Self)
        } else {
            Err(UnknownRecordVersion(version))
        }
    }
}

impl From<RecordVersion> for u32 {
    fn from(_: RecordVersion) -> Self {
        DRIVE_RECORD_VERSION
    }
}

/// Where the archive of a call folder stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum ArchiveState {
    /// Uploads under way. Nothing is deleted in this state.
    Uploading,
    /// Every planned file is verified on Drive and listed. Deletion may have started.
    Uploaded,
    /// The local copies of `files` and `mixed_stem_names` are gone.
    MediaDeleted { deleted_at: Timestamp },
}

impl ArchiveState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uploading => "uploading",
            Self::Uploaded => "uploaded",
            Self::MediaDeleted { .. } => "media_deleted",
        }
    }
}

/// What one uploaded file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchivedFileKind {
    /// `call.mp4` as it is.
    Video,
    /// A WAV stem as lossless FLAC.
    Stem,
}

/// One uploaded file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedFile {
    /// The file in the call folder: `call.mp4` or a stem such as `001-mic.wav`.
    pub local_name: String,
    /// `call.mp4` or a stem such as `001-mic.flac`.
    pub drive_name: String,
    pub kind: ArchivedFileKind,
    pub file_id: String,
    /// Of the Drive copy.
    pub size_bytes: u64,
    /// Lowercase hex MD5 of the Drive copy.
    pub md5: String,
    /// Lowercase hex MD5 of the local file. For a FLAC it is the original WAV's, so a restore can
    /// prove it rebuilt the same bytes.
    pub local_md5: String,
    /// As Drive returned it. Read through `drive_link`.
    pub web_view_link: Option<String>,
    pub uploaded_at: Timestamp,
}

/// Every Drive copy's link starts so. `drive.json` is a local file anyone may edit, and its link
/// goes into notes and to the browser.
const DRIVE_LINK_PREFIX: &str = "https://drive.google.com/";

impl ArchivedFile {
    /// The Drive copy's page, or `None` when the record has none or it is not on Drive.
    #[must_use]
    pub fn drive_link(&self) -> Option<&str> {
        self.web_view_link
            .as_deref()
            .filter(|link| link.starts_with(DRIVE_LINK_PREFIX))
    }
}

/// Where a call folder's media stands against Drive, for the steps that must not touch archived
/// media.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveStanding {
    /// No `drive.json`.
    Local,
    Uploading,
    /// Every upload is recorded; the local copies may be partly or wholly there.
    Uploaded,
    MediaDeleted,
    /// `drive.json` is there but cannot be read. The media may be gone, so it counts as archived.
    Unreadable,
}

impl DriveStanding {
    /// The folder's standing. A record that cannot be read is logged.
    #[must_use]
    pub fn of(folder: &Path) -> Self {
        let path = folder.join(DRIVE_RECORD_FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::Local,
            Err(error) => {
                tracing::warn!(file.path = %path.display(), exception.message = %error, "drive_record.unreadable");
                return Self::Unreadable;
            }
        };
        match serde_json::from_str::<DriveRecord<serde::de::IgnoredAny>>(&text) {
            Ok(record) => match record.state {
                ArchiveState::Uploading => Self::Uploading,
                ArchiveState::Uploaded => Self::Uploaded,
                ArchiveState::MediaDeleted { .. } => Self::MediaDeleted,
            },
            Err(error) => {
                tracing::warn!(file.path = %path.display(), exception.message = %error, "drive_record.unreadable");
                Self::Unreadable
            }
        }
    }

    /// Whether finalize, repair, the purge and the fragment cleanup leave the folder alone.
    #[must_use]
    pub fn is_archived(self) -> bool {
        match self {
            Self::Uploaded | Self::MediaDeleted | Self::Unreadable => true,
            Self::Local | Self::Uploading => false,
        }
    }
}

/// Whether the folder's media is all on Drive, deleted locally or not, or its record cannot be
/// read.
#[must_use]
pub fn is_archived(folder: &Path) -> bool {
    DriveStanding::of(folder).is_archived()
}

#[derive(Debug, thiserror::Error)]
pub enum DriveRecordError {
    #[error("could not read or write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a drive record: {source}", path.display())]
    Format {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl<Session> DriveRecord<Session> {
    /// The folder's record, `None` when it has none.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or is not a record of this layout.
    pub fn read(folder: &Path) -> Result<Option<Self>, DriveRecordError>
    where
        Session: serde::de::DeserializeOwned,
    {
        let path = folder.join(DRIVE_RECORD_FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(DriveRecordError::Io { path, source }),
        };
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|source| DriveRecordError::Format { path, source })
    }

    /// Written beside, synced, renamed, then the folder synced, so a reader never sees half a
    /// record and the rename survives a power cut before the deletion that follows.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written, synced or renamed.
    pub fn write(&self, folder: &Path) -> Result<(), DriveRecordError>
    where
        Session: Serialize,
    {
        let path = folder.join(DRIVE_RECORD_FILE_NAME);
        let text =
            serde_json::to_string_pretty(self).map_err(|source| DriveRecordError::Format {
                path: path.clone(),
                source,
            })?;
        let partial = folder.join(format!(".{DRIVE_RECORD_FILE_NAME}.partial"));
        let io_error = |source| DriveRecordError::Io {
            path: path.clone(),
            source,
        };
        let mut file = std::fs::File::create(&partial).map_err(io_error)?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(io_error)?;
        drop(file);
        std::fs::rename(&partial, &path).map_err(io_error)?;
        crate::sync_folder(folder).map_err(io_error)
    }
}
