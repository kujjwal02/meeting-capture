//! Which media of a call folder the Drive archive may take, by the rules the fragment cleanup
//! uses: the call ended over `FRAGMENT_KEEP_AFTER_CALL_END` ago on the purge clock, the app does
//! not hold it open, and finalize is done with it.
//!
//! A folder with `call.mp4` written from the current manifests, complete and of the length
//! finalize recorded, archives as video, with each stem its mix left out. A folder with no video
//! to place, or whose video the 90-day purge took, archives as stems. Anything else keeps its
//! media, with the reason.

use std::collections::BTreeSet;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jiff::{SignedDuration, Timestamp};

use call_folder::drive_record::DriveStanding;

use crate::purge::{
    FolderLocks, advance_purge_clock, ended_at, has_deletable_segment_folder, has_purge_record,
    is_call_file_as_recorded, read_purge_clock, take_folder_locks, timestamp_of,
};
use crate::record::{FinalizeStanding, finalize_standing};
use crate::{FRAGMENT_KEEP_AFTER_CALL_END, FinalizeError, OUTPUT_FILE_NAME, plan_folder};

/// A call folder the archive may take now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivableCall {
    pub kind: ArchiveKind,
    pub call_ended_at: Timestamp,
}

/// Which media of the call goes to Drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveKind {
    /// `call.mp4`, which holds the mix of `mixed_stem_names`. Any other stem goes as FLAC.
    Video { mixed_stem_names: BTreeSet<String> },
    /// Each WAV stem as lossless FLAC, for calls with no `call.mp4`.
    Stems,
}

impl ArchiveKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Video { .. } => "video",
            Self::Stems => "stems",
        }
    }
}

/// Why a call folder keeps its media for now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveHold {
    /// `drive.json` records every upload, or cannot be read; the pipeline finishes the rest.
    AlreadyArchived,
    /// The folder does not resolve to a child of the calls folder.
    OutsideCalls,
    /// The app holds the call open; a rejoin would record into the folder again.
    CallOpen,
    /// No purge has run yet, so there is no trusted clock to age the call by.
    NoPurgeClock,
    /// No stem, manifest or fragment, so the call has no end.
    NoCaptureFile,
    EndedRecently {
        call_ended_at: Timestamp,
    },
    /// A capture process or a finalize holds a folder lock.
    Busy,
    FinalizePending,
    FinalizeFailed,
    /// `call.mp4` differs in length from what finalize recorded.
    CallFileMismatch,
    /// `call.mp4`'s top-level boxes do not run to its end, or lack `moov` or `mdat`.
    CallFileIncomplete,
    /// The manifests could not be planned again, so which stems `call.mp4` mixed is unknown.
    MixUnknown {
        message: String,
    },
    /// The fragment cleanup has not deleted the fragments `call.mp4` holds yet.
    FragmentsLeft,
    /// The 90-day purge started on the folder and left `call.mp4`; its next run finishes it.
    PurgeUnfinished,
    /// Finalize recorded no video, yet a `call.mp4` is there.
    StaleCallFile,
}

impl ArchiveHold {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AlreadyArchived => "already_archived",
            Self::OutsideCalls => "outside_calls",
            Self::CallOpen => "call_open",
            Self::NoPurgeClock => "no_purge_clock",
            Self::NoCaptureFile => "no_capture_file",
            Self::EndedRecently { .. } => "ended_recently",
            Self::Busy => "busy",
            Self::FinalizePending => "finalize_pending",
            Self::FinalizeFailed => "finalize_failed",
            Self::CallFileMismatch => "call_file_mismatch",
            Self::CallFileIncomplete => "call_file_incomplete",
            Self::MixUnknown { .. } => "mix_unknown",
            Self::FragmentsLeft => "fragments_left",
            Self::PurgeUnfinished => "purge_unfinished",
            Self::StaleCallFile => "stale_call_file",
        }
    }
}

/// Whether the archive may take the folder's media now, and as what. `open_call_folders` are
/// the folders of calls the app holds open. Reads the purge clock and never moves it. Takes no
/// lock: the caller checks again under `take_media_locks` before it deletes.
///
/// # Errors
///
/// Fails when the calls folder or the call folder cannot be resolved or read.
pub fn archive_readiness(
    calls_dir: &Path,
    folder: &Path,
    open_call_folders: &[PathBuf],
    now: SystemTime,
) -> Result<Result<ArchivableCall, ArchiveHold>, FinalizeError> {
    let resolve = |path: &Path| {
        path.canonicalize().map_err(|source| FinalizeError::Io {
            path: path.to_owned(),
            source,
        })
    };
    let root = resolve(calls_dir)?;
    let folder = resolve(folder)?;
    if folder.parent() != Some(root.as_path()) {
        return Ok(Err(ArchiveHold::OutsideCalls));
    }
    match DriveStanding::of(&folder) {
        DriveStanding::Uploaded | DriveStanding::MediaDeleted | DriveStanding::Unreadable => {
            return Ok(Err(ArchiveHold::AlreadyArchived));
        }
        DriveStanding::Local | DriveStanding::Uploading => {}
    }
    // A call folder not created yet is not this one.
    let open_call_folders: BTreeSet<PathBuf> = open_call_folders
        .iter()
        .filter_map(|open| open.canonicalize().ok())
        .collect();
    if open_call_folders.contains(&folder) {
        return Ok(Err(ArchiveHold::CallOpen));
    }
    let Some(clock) = read_purge_clock(&root) else {
        return Ok(Err(ArchiveHold::NoPurgeClock));
    };
    let trusted_now = advance_purge_clock(&clock, timestamp_of(now, &root)?);
    let keep =
        SignedDuration::try_from(FRAGMENT_KEEP_AFTER_CALL_END).unwrap_or(SignedDuration::MAX);
    let Some(call_ended_at) = ended_at(&folder)? else {
        return Ok(Err(ArchiveHold::NoCaptureFile));
    };
    let call_ended_at = timestamp_of(call_ended_at, &folder)?;
    if trusted_now
        .checked_sub(keep)
        .is_ok_and(|cutoff| call_ended_at >= cutoff)
    {
        return Ok(Err(ArchiveHold::EndedRecently { call_ended_at }));
    }
    let has_call_file = folder.join(OUTPUT_FILE_NAME).exists();
    let kind = if has_purge_record(&folder) {
        if has_call_file {
            return Ok(Err(ArchiveHold::PurgeUnfinished));
        }
        ArchiveKind::Stems
    } else if !folder.join(video_capture::MANIFEST_FILE_NAME).exists() {
        // An audio-only call: finalize has nothing to write, so the stems are all there is.
        if has_call_file {
            return Ok(Err(ArchiveHold::StaleCallFile));
        }
        ArchiveKind::Stems
    } else {
        match finalize_standing(&folder) {
            FinalizeStanding::Written => match video_kind(&folder)? {
                Ok(kind) => kind,
                Err(hold) => return Ok(Err(hold)),
            },
            FinalizeStanding::NoVideo if has_call_file => {
                return Ok(Err(ArchiveHold::StaleCallFile));
            }
            FinalizeStanding::NoVideo => ArchiveKind::Stems,
            FinalizeStanding::Pending => return Ok(Err(ArchiveHold::FinalizePending)),
            FinalizeStanding::Failed => return Ok(Err(ArchiveHold::FinalizeFailed)),
        }
    };
    // The fragment cleanup deletes what call.mp4 holds; the archive leaves that job to it.
    if has_deletable_segment_folder(&folder)? {
        return Ok(Err(ArchiveHold::FragmentsLeft));
    }
    Ok(Ok(ArchivableCall {
        kind,
        call_ended_at,
    }))
}

/// The video kind for a folder whose finalize wrote `call.mp4` from the current manifests, with
/// the stems that run mixed. Finalize leaves out stems it cannot place, such as a segment with no
/// clock reading; planning the same manifests again names the ones it placed.
fn video_kind(folder: &Path) -> Result<Result<ArchiveKind, ArchiveHold>, FinalizeError> {
    if !is_call_file_as_recorded(folder)? {
        return Ok(Err(ArchiveHold::CallFileMismatch));
    }
    let path = folder.join(OUTPUT_FILE_NAME);
    let is_complete = is_complete_mp4(&path).map_err(|source| FinalizeError::Io {
        path: path.clone(),
        source,
    })?;
    if !is_complete {
        tracing::warn!(file.path = %path.display(), "archive.call_file_incomplete");
        return Ok(Err(ArchiveHold::CallFileIncomplete));
    }
    let timeline = match plan_folder(folder) {
        Ok(timeline) => timeline,
        Err(error) => {
            return Ok(Err(ArchiveHold::MixUnknown {
                message: error.to_string(),
            }));
        }
    };
    let mixed_stem_names = timeline
        .stems
        .iter()
        .filter(|stem| !stem.chunks.is_empty())
        .filter_map(|stem| stem.file.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    Ok(Ok(ArchiveKind::Video { mixed_stem_names }))
}

/// Whether the file's top-level MP4 boxes run from its start to exactly its end, and include
/// `moov` and `mdat`. A file cut short or still being written fails, whatever its length.
fn is_complete_mp4(path: &Path) -> std::io::Result<bool> {
    const BOX_HEADER_BYTES: u64 = 8;
    const LARGE_BOX_HEADER_BYTES: u64 = 16;
    let mut file = std::fs::File::open(path)?;
    let file_bytes = file.metadata()?.len();
    let mut offset = 0_u64;
    let mut has_moov = false;
    let mut has_mdat = false;
    while offset < file_bytes {
        let left_bytes = file_bytes - offset;
        if left_bytes < BOX_HEADER_BYTES {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header)?;
        let size_field = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let box_type = &header[4..8];
        let box_bytes = match size_field {
            // The last box runs to the end of the file.
            0 => left_bytes,
            // A 64-bit size follows the type.
            1 => {
                if left_bytes < LARGE_BOX_HEADER_BYTES {
                    return Ok(false);
                }
                let mut large_size = [0_u8; 8];
                file.read_exact(&mut large_size)?;
                let large_bytes = u64::from_be_bytes(large_size);
                if large_bytes < LARGE_BOX_HEADER_BYTES {
                    return Ok(false);
                }
                large_bytes
            }
            size => u64::from(size),
        };
        if box_bytes < BOX_HEADER_BYTES || box_bytes > left_bytes {
            return Ok(false);
        }
        has_moov |= box_type == b"moov";
        has_mdat |= box_type == b"mdat";
        offset += box_bytes;
    }
    Ok(has_moov && has_mdat)
}

/// `audio.lock`, `video.lock` and `finalize.lock`, held while the archive deletes or a restore
/// writes, so no capture process, finalize, purge or fragment cleanup touches the folder.
pub struct MediaLocks {
    _locks: FolderLocks,
}

/// The folder's three locks, or `None` when another process holds any of them.
///
/// # Errors
///
/// Fails when a lock file cannot be opened or the OS lock call fails.
pub fn take_media_locks(folder: &Path) -> Result<Option<MediaLocks>, FinalizeError> {
    Ok(take_folder_locks(folder)?
        .ok()
        .map(|locks| MediaLocks { _locks: locks }))
}
