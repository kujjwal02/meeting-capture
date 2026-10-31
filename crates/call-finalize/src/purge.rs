//! The 90-day purge of call video: `call.mp4` and the `NNN-video` fragment folders of each call
//! folder whose call ended more than `MEDIA_RETENTION_DAYS` ago. The WAV stems, both manifests,
//! the finalize record and every other file stay: phase 4 transcribes the stems later.
//!
//! A call ended at the newest change to a capture file: a stem, `audio.json`, `video.json` or a
//! file in an `NNN-video` folder. Finalize's own output does not count, so a late finalize does
//! not move the end. The purge holds `audio.lock`, `video.lock` and `finalize.lock` while it
//! works, and skips a folder where any of them is taken.
//!
//! Fragments stay until a finalize succeeds. The purge deletes only where the finalize record
//! says `call.mp4` is written from the current manifests and is still there, or that they hold
//! no video to place. A failed or killed finalize with no retry left, or fragments with no
//! `video.json`, keep everything and are listed as `finalize_failed`. A folder still waiting for
//! its `call.mp4` keeps everything too: the next launch finalizes it, and a later purge takes it.
//! A segment that finalize skipped for any reason but `Empty`, or that `video.json` does not
//! list, never reached `call.mp4`, so its folder stays.
//!
//! The purge record, `media-purge.json`, is written and its folder synced before anything is
//! deleted. A run killed in the middle leaves it, so finalize refuses the folder, and the next
//! purge deletes the rest.
//!
//! The purge ages calls against its own clock, kept in `media-purge-run.json` in the calls
//! folder. Each run moves it forward by the wall time since the last run, at most
//! `MAX_CLOCK_STEP`, and never past the wall clock; a clock set back pulls it back at once. The
//! first run, or any run that finds the record lost, starts it at the newest capture file, and
//! moves every first-seen stamp up to it. A clock set a year ahead so ages calls by two days, not
//! a year, and calls must be `MAX_CLOCK_STEP` older than the retention, so one jump deletes
//! nothing early. Jumps repeated without the clock set right are not bounded. The cost is lag:
//! each run that finds the wall clock moved more than two days leaves the purge clock behind by
//! the excess, so a Mac off for a week deletes about five days late from then on.
//!
//! A clock set back makes the calls recorded meanwhile look old. So the first run that sees a
//! call folder with video stamps it with `media-first-seen.json` at the purge clock, and a call
//! ages from the later of its end and that stamp. The run that brings the purge to a folder of
//! old calls deletes none of them: they go 90 days later.
//!
//! Deletion never follows a symbolic link. Call folders come from `call_folder::call_folders`,
//! which lists real folders only, and each must resolve to a direct child of the calls folder.
//! A `call.mp4` or `NNN-video` that is a link is left in place and counted. Segment folders go
//! through `remove_dir_all`, which removes links inside them, not their targets.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use call_folder::FolderLock;
use capture_protocol::finalize::{KeepReason, KeptCall, MediaPurge, PurgedCall};
use video_capture::{VideoManifest, VideoSegmentState};

use crate::record::{FinalizeStanding, finalize_standing, recorded_skipped_video};
use crate::{FinalizeError, LOCK_FILE_NAME, OUTPUT_FILE_NAME, SkipReason};

/// How long a call's video stays after the call ends. Phase 7 moves it to Drive first.
pub const MEDIA_RETENTION_DAYS: u64 = 90;

/// In each purged call folder.
pub const PURGE_RECORD_FILE_NAME: &str = "media-purge.json";

/// In each call folder with video: when a purge run first saw it.
pub const FIRST_SEEN_FILE_NAME: &str = "media-first-seen.json";

/// In the calls folder: the purge clock.
pub const PURGE_RUN_FILE_NAME: &str = "media-purge-run.json";

/// The furthest the purge clock moves in one run, twice the app's purge interval.
const MAX_CLOCK_STEP: Duration = Duration::from_hours(48);

/// A purge clock further behind the wall clock than this is logged.
const CLOCK_LAG_WARNING: Duration = Duration::from_hours(1);

/// Raised when the layout changes in a way an older reader would misread.
const PURGE_RECORD_SCHEMA_VERSION: u32 = 1;

/// Version 1 stored the last run's wall time alone.
const PURGE_RUN_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
struct PurgeRecord {
    schema_version: u32,
    purged_at: Timestamp,
    call_ended_at: Timestamp,
}

#[derive(Debug, Serialize, Deserialize)]
struct FirstSeenRecord {
    schema_version: u32,
    first_seen_at: Timestamp,
}

/// The purge clock, both times UTC.
#[derive(Debug, Serialize, Deserialize)]
struct PurgeRunRecord {
    schema_version: u32,
    /// The wall clock at the last run.
    last_clock_at: Timestamp,
    /// The purge clock after the last run.
    trusted_now: Timestamp,
}

/// One call folder's outcome. Folders that are too young or hold no video get none.
enum FolderPurge {
    Purged(PurgedCall),
    Kept(KeepReason),
}

/// What one deletion pass over a folder removed and left.
#[derive(Default)]
struct DeletionCount {
    file_count: u64,
    bytes: u64,
    refused_entry_count: u64,
}

/// Deletes the video of every call in `calls_dir` older than `MEDIA_RETENTION_DAYS` on the
/// purge clock, which this run moves toward `now`. A folder that fails is kept and listed with
/// its error; the others go on.
///
/// # Errors
///
/// Fails when `calls_dir` exists but cannot be resolved or listed, or its run record cannot be
/// written.
pub fn purge_expired_media(calls_dir: &Path, now: SystemTime) -> Result<MediaPurge, FinalizeError> {
    let mut purge = MediaPurge {
        trusted_now: None,
        first_seen_count: 0,
        purged: Vec::new(),
        kept: Vec::new(),
    };
    let io_error = |source| FinalizeError::Io {
        path: calls_dir.to_owned(),
        source,
    };
    if !calls_dir.is_dir() {
        return Ok(purge);
    }
    let folders = call_folder::call_folders(calls_dir).map_err(io_error)?;
    let root = calls_dir.canonicalize().map_err(io_error)?;
    let now_at = timestamp_of(now, calls_dir)?;
    let (trusted_now, is_reseeded) = match read_purge_clock(calls_dir) {
        Some(clock) => (advance_purge_clock(&clock, now_at), false),
        None => (
            newest_capture_time(&folders).map_or(now_at, |newest| newest.min(now_at)),
            true,
        ),
    };
    write_atomically(
        calls_dir,
        PURGE_RUN_FILE_NAME,
        &PurgeRunRecord {
            schema_version: PURGE_RUN_SCHEMA_VERSION,
            last_clock_at: now_at,
            trusted_now,
        },
    )?;
    let lag = now_at.duration_since(trusted_now);
    if lag > SignedDuration::try_from(CLOCK_LAG_WARNING).unwrap_or(SignedDuration::MAX) {
        tracing::warn!(
            file.directory = %calls_dir.display(),
            purge.trusted_now = %trusted_now,
            purge.wall_clock_at = %now_at,
            purge.clock_lag_s = lag.as_secs(),
            "purge.clock_lagging"
        );
    }
    purge.trusted_now = Some(trusted_now);
    // One forward jump can put the purge clock up to one step ahead of real time.
    let retention_hours =
        (MEDIA_RETENTION_DAYS * 24).saturating_add(MAX_CLOCK_STEP.as_secs() / 3600);
    let retention = SignedDuration::from_hours(i64::try_from(retention_hours).unwrap_or(i64::MAX));
    let Ok(cutoff) = trusted_now.checked_sub(retention) else {
        return Ok(purge);
    };
    for folder in folders {
        let outcome = purge_folder(&root, &folder, trusted_now, is_reseeded, cutoff)
            .unwrap_or_else(|error| FolderSight {
                is_first_seen: false,
                outcome: Some(FolderPurge::Kept(KeepReason::Failed {
                    message: error.to_string(),
                })),
            });
        if outcome.is_first_seen {
            purge.first_seen_count += 1;
        }
        match outcome.outcome {
            Some(FolderPurge::Purged(call)) => purge.purged.push(call),
            Some(FolderPurge::Kept(reason)) => {
                tracing::warn!(
                    file.directory = %folder.display(),
                    purge.keep_reason = reason.as_str(),
                    exception.message = match &reason {
                        KeepReason::Failed { message } => Some(message.as_str()),
                        KeepReason::Recording
                        | KeepReason::Finalizing
                        | KeepReason::FinalizePending
                        | KeepReason::FinalizeFailed
                        | KeepReason::UnplacedVideo { .. }
                        | KeepReason::OpenVideoSegment
                        | KeepReason::OutsideRoot => None,
                    },
                    "purge.folder_kept"
                );
                purge.kept.push(KeptCall { folder, reason });
            }
            None => {}
        }
    }
    Ok(purge)
}

/// Whether a purge started deleting this folder's video. Finalize then has nothing to work on.
pub(crate) fn has_purge_record(folder: &Path) -> bool {
    folder.join(PURGE_RECORD_FILE_NAME).exists()
}

/// An unreadable, older or newer record counts as none: the clock starts again from the newest
/// capture file, which never ages a call past its real age.
fn read_purge_clock(calls_dir: &Path) -> Option<PurgeRunRecord> {
    let text = std::fs::read_to_string(calls_dir.join(PURGE_RUN_FILE_NAME)).ok()?;
    serde_json::from_str::<PurgeRunRecord>(&text)
        .ok()
        .filter(|record| record.schema_version == PURGE_RUN_SCHEMA_VERSION)
}

/// The wall time since the last run, clamped to `0..=MAX_CLOCK_STEP`, added to the purge
/// clock, and never past the wall clock.
fn advance_purge_clock(clock: &PurgeRunRecord, now: Timestamp) -> Timestamp {
    let max_step = SignedDuration::try_from(MAX_CLOCK_STEP).unwrap_or(SignedDuration::MAX);
    let step = now
        .duration_since(clock.last_clock_at)
        .clamp(SignedDuration::ZERO, max_step);
    clock
        .trusted_now
        .checked_add(step)
        .map_or(now, |advanced| advanced.min(now))
}

/// The newest capture file across the call folders. A folder that cannot be read is left out.
fn newest_capture_time(folders: &[PathBuf]) -> Option<Timestamp> {
    folders
        .iter()
        .filter_map(|folder| ended_at(folder).ok().flatten())
        .max()
        .and_then(|newest| Timestamp::try_from(newest).ok())
}

/// The folder's first-seen stamp, written now at `trusted_now` when it has none. A reseeded
/// purge clock came from capture times that a wrong clock may have written, so it also moves an
/// earlier stamp up to `trusted_now`: a lost run record then only delays deletion. `true` with a
/// stamp this run wrote.
fn first_seen_at(
    folder: &Path,
    trusted_now: Timestamp,
    is_reseeded: bool,
) -> Result<(Timestamp, bool), FinalizeError> {
    let path = folder.join(FIRST_SEEN_FILE_NAME);
    let recorded = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<FirstSeenRecord>(&text).ok())
        .filter(|record| record.schema_version <= PURGE_RECORD_SCHEMA_VERSION);
    if let Some(record) = recorded
        && (!is_reseeded || record.first_seen_at >= trusted_now)
    {
        return Ok((record.first_seen_at, false));
    }
    // An unreadable stamp is written again: a later stamp only delays the purge.
    write_atomically(
        folder,
        FIRST_SEEN_FILE_NAME,
        &FirstSeenRecord {
            schema_version: PURGE_RECORD_SCHEMA_VERSION,
            first_seen_at: trusted_now,
        },
    )?;
    tracing::info!(
        file.directory = %folder.display(),
        purge.first_seen_at = %trusted_now,
        "purge.folder_first_seen"
    );
    Ok((trusted_now, true))
}

/// One folder's outcome, and whether this run stamped it first-seen.
struct FolderSight {
    is_first_seen: bool,
    outcome: Option<FolderPurge>,
}

fn purge_folder(
    root: &Path,
    folder: &Path,
    trusted_now: Timestamp,
    is_reseeded: bool,
    cutoff: Timestamp,
) -> Result<FolderSight, FinalizeError> {
    let folder = folder.canonicalize().map_err(|source| FinalizeError::Io {
        path: folder.to_owned(),
        source,
    })?;
    let sight = |is_first_seen, outcome| FolderSight {
        is_first_seen,
        outcome,
    };
    if folder.parent() != Some(root) {
        return Ok(sight(
            false,
            Some(FolderPurge::Kept(KeepReason::OutsideRoot)),
        ));
    }
    // Cheap checks first, so young folders are never locked.
    if video_media(&folder)?.is_empty() {
        return Ok(sight(false, None));
    }
    let (first_seen_at, is_first_seen) = first_seen_at(&folder, trusted_now, is_reseeded)?;
    if !is_aged_before(&folder, first_seen_at, cutoff)? {
        return Ok(sight(is_first_seen, None));
    }
    let outcome = purge_aged_folder(&folder, first_seen_at, cutoff)?;
    Ok(sight(is_first_seen, outcome))
}

/// The purge of a folder whose call looked old enough before its locks were taken.
fn purge_aged_folder(
    folder: &Path,
    first_seen_at: Timestamp,
    cutoff: Timestamp,
) -> Result<Option<FolderPurge>, FinalizeError> {
    let lock = |name: &str| {
        FolderLock::try_take(folder, name).map_err(|source| FinalizeError::Io {
            path: folder.join(name),
            source,
        })
    };
    let Some(_audio_lock) = lock(stem_writer::LOCK_FILE_NAME)? else {
        return Ok(Some(FolderPurge::Kept(KeepReason::Recording)));
    };
    let Some(_video_lock) = lock(video_capture::LOCK_FILE_NAME)? else {
        return Ok(Some(FolderPurge::Kept(KeepReason::Recording)));
    };
    let Some(_finalize_lock) = lock(LOCK_FILE_NAME)? else {
        return Ok(Some(FolderPurge::Kept(KeepReason::Finalizing)));
    };
    // Again under the locks: a writer may have finished in between.
    if !is_aged_before(folder, first_seen_at, cutoff)? {
        return Ok(None);
    }
    let Some(call_ended_at) = ended_at(folder)? else {
        return Ok(None);
    };
    if has_open_video_segment(folder)? {
        return Ok(Some(FolderPurge::Kept(KeepReason::OpenVideoSegment)));
    }
    // A record means an earlier purge passed this check and was cut short.
    if !has_purge_record(folder) {
        match finalize_standing(folder) {
            FinalizeStanding::Succeeded => {}
            FinalizeStanding::Pending => {
                return Ok(Some(FolderPurge::Kept(KeepReason::FinalizePending)));
            }
            FinalizeStanding::Failed => {
                return Ok(Some(FolderPurge::Kept(KeepReason::FinalizeFailed)));
            }
        }
    }
    let Some(unplaced) = unplaced_segments(folder)? else {
        return Ok(Some(FolderPurge::Kept(KeepReason::FinalizeFailed)));
    };
    let deletable: Vec<PathBuf> = video_media(folder)?
        .into_iter()
        .filter(|path| segment_number_of(path).is_none_or(|segment| !unplaced.contains(&segment)))
        .collect();
    let unplaced_segments: Vec<u32> = unplaced.into_iter().collect();
    if deletable.is_empty() {
        return Ok(Some(FolderPurge::Kept(KeepReason::UnplacedVideo {
            segments: unplaced_segments,
        })));
    }
    let call_ended_at = timestamp_of(call_ended_at, folder)?;
    write_atomically(
        folder,
        PURGE_RECORD_FILE_NAME,
        &PurgeRecord {
            schema_version: PURGE_RECORD_SCHEMA_VERSION,
            purged_at: Timestamp::now(),
            call_ended_at,
        },
    )?;
    let deleted = delete_video_media(&deletable, call_ended_at)?;
    tracing::info!(
        file.directory = %folder.display(),
        purge.call_ended_at = %call_ended_at,
        purge.first_seen_at = %first_seen_at,
        purge.deleted_file_count = deleted.file_count,
        purge.deleted_bytes = deleted.bytes,
        purge.refused_entry_count = deleted.refused_entry_count,
        purge.unplaced_segment_count = unplaced_segments.len(),
        "purge.folder_purged"
    );
    Ok(Some(FolderPurge::Purged(PurgedCall {
        folder: folder.to_owned(),
        call_ended_at,
        first_seen_at,
        deleted_file_count: deleted.file_count,
        deleted_bytes: deleted.bytes,
        refused_entry_count: deleted.refused_entry_count,
        unplaced_segments,
    })))
}

/// The `NNN-video` folders whose frames `call.mp4` does not hold: those the last finished
/// finalize skipped for a reason other than `Empty`, and those `video.json` does not list.
/// `None` when no finished finalize record can be read.
fn unplaced_segments(folder: &Path) -> Result<Option<BTreeSet<u32>>, FinalizeError> {
    let Some(skipped) = recorded_skipped_video(folder) else {
        return Ok(None);
    };
    let mut unplaced: BTreeSet<u32> = skipped
        .iter()
        .filter(|skip| match skip.reason {
            SkipReason::Empty => false,
            SkipReason::StillOpen | SkipReason::NoClock | SkipReason::NoPartRecord => true,
        })
        .map(|skip| skip.segment)
        .collect();
    let listed: BTreeSet<u32> = VideoManifest::load_or_new(folder)?
        .segments
        .iter()
        .map(|segment| segment.number)
        .collect();
    unplaced.extend(
        video_media(folder)?
            .iter()
            .filter_map(|path| segment_number_of(path))
            .filter(|segment| !listed.contains(segment)),
    );
    Ok(Some(unplaced))
}

fn segment_number_of(path: &Path) -> Option<u32> {
    path.file_name()
        .and_then(|name| video_capture::segment_number_of_folder(&name.to_string_lossy()))
}

/// The video entries directly in the folder: `call.mp4`, what a killed finalize left, and each
/// `NNN-video` folder. Links among them are listed too, so the deletion can refuse and count
/// them.
fn video_media(folder: &Path) -> Result<Vec<PathBuf>, FinalizeError> {
    let io_error = |source| FinalizeError::Io {
        path: folder.to_owned(),
        source,
    };
    let mut media = Vec::new();
    for entry in std::fs::read_dir(folder).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_video_name(&name) {
            media.push(entry.path());
        }
    }
    media.sort();
    Ok(media)
}

fn is_video_name(name: &str) -> bool {
    name == OUTPUT_FILE_NAME
        || crate::is_stray_name(name)
        || video_capture::segment_number_of_folder(name).is_some()
}

/// A stem, such as `001-mic.wav`.
fn is_stem_name(name: &str) -> bool {
    let is_wav = Path::new(name)
        .extension()
        .is_some_and(|extension| extension == "wav");
    is_wav
        && name.split_once('-').is_some_and(|(digits, _)| {
            digits.len() >= 3 && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Whether the call's age, counted from the later of its end and its first-seen stamp, passed
/// the cutoff. A folder with no capture file has no end, so it never ages.
fn is_aged_before(
    folder: &Path,
    first_seen_at: Timestamp,
    cutoff: Timestamp,
) -> Result<bool, FinalizeError> {
    let Some(ended) = ended_at(folder)? else {
        return Ok(false);
    };
    let ended = timestamp_of(ended, folder)?;
    Ok(ended.max(first_seen_at) < cutoff)
}

/// The newest modified time of the capture files: the stems, both manifests and the files in the
/// `NNN-video` folders, links not followed. `None` when the folder has none.
fn ended_at(folder: &Path) -> Result<Option<SystemTime>, FinalizeError> {
    let mut newest: Option<SystemTime> = None;
    let mut track_newest_modified = |path: &Path| -> Result<(), FinalizeError> {
        let modified = std::fs::symlink_metadata(path)
            .and_then(|metadata| metadata.modified())
            .map_err(|source| FinalizeError::Io {
                path: path.to_owned(),
                source,
            })?;
        newest = Some(newest.map_or(modified, |newest| newest.max(modified)));
        Ok(())
    };
    let folder_error = |source| FinalizeError::Io {
        path: folder.to_owned(),
        source,
    };
    for entry in std::fs::read_dir(folder).map_err(folder_error)? {
        let entry = entry.map_err(folder_error)?;
        let path = entry.path();
        let entry_error = |source| FinalizeError::Io {
            path: path.clone(),
            source,
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let file_type = entry.file_type().map_err(entry_error)?;
        if file_type.is_dir() {
            if video_capture::segment_number_of_folder(&name).is_none() {
                continue;
            }
            for inner in std::fs::read_dir(&path).map_err(entry_error)? {
                let inner = inner.map_err(entry_error)?;
                let inner_path = inner.path();
                let is_dir = inner.file_type().map_err(|source| FinalizeError::Io {
                    path: inner_path.clone(),
                    source,
                })?;
                if !is_dir.is_dir() {
                    track_newest_modified(&inner_path)?;
                }
            }
        } else if name == stem_writer::MANIFEST_FILE_NAME
            || name == video_capture::MANIFEST_FILE_NAME
            || is_stem_name(&name)
        {
            track_newest_modified(&path)?;
        }
    }
    Ok(newest)
}

/// Whether `video.json` lists a segment no writer closed and repair has not recovered.
fn has_open_video_segment(folder: &Path) -> Result<bool, FinalizeError> {
    let manifest = VideoManifest::load_or_new(folder)?;
    Ok(manifest
        .segments
        .iter()
        .any(|segment| segment.state == VideoSegmentState::Open))
}

fn timestamp_of(time: SystemTime, path: &Path) -> Result<Timestamp, FinalizeError> {
    Timestamp::try_from(time).map_err(|error| FinalizeError::Io {
        path: path.to_owned(),
        source: std::io::Error::other(error),
    })
}

/// Written beside, renamed, then the folder synced, so a reader never sees half a record and the
/// rename survives a power cut before any deletion that follows.
fn write_atomically(
    folder: &Path,
    file_name: &str,
    record: &impl Serialize,
) -> Result<(), FinalizeError> {
    let path = folder.join(file_name);
    let text = serde_json::to_string_pretty(record).map_err(|source| FinalizeError::Record {
        path: path.clone(),
        source,
    })?;
    let partial = folder.join(format!(".{file_name}.partial"));
    let io_error = |source| FinalizeError::Io {
        path: path.clone(),
        source,
    };
    let mut file = std::fs::File::create(&partial).map_err(io_error)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io_error)?;
    std::fs::rename(&partial, &path).map_err(io_error)?;
    sync_folder(folder)
}

#[cfg(unix)]
fn sync_folder(folder: &Path) -> Result<(), FinalizeError> {
    std::fs::File::open(folder)
        .and_then(|handle| handle.sync_all())
        .map_err(|source| FinalizeError::Io {
            path: folder.to_owned(),
            source,
        })
}

/// NTFS journals the rename itself, and Windows opens no folder handle to sync.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the same signature as the Unix version"
)]
fn sync_folder(_folder: &Path) -> Result<(), FinalizeError> {
    Ok(())
}

fn delete_video_media(
    media: &[PathBuf],
    call_ended_at: Timestamp,
) -> Result<DeletionCount, FinalizeError> {
    let mut deleted = DeletionCount::default();
    for path in media {
        let io_error = |source| FinalizeError::Io {
            path: path.clone(),
            source,
        };
        let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
        let file_type = metadata.file_type();
        if file_type.is_file() {
            std::fs::remove_file(path).map_err(io_error)?;
            deleted.file_count += 1;
            deleted.bytes += metadata.len();
            tracing::info!(
                file.path = %path.display(),
                file.size = metadata.len(),
                purge.call_ended_at = %call_ended_at,
                "purge.file_deleted"
            );
        } else if file_type.is_dir() {
            delete_segment_folder(path, call_ended_at, &mut deleted)?;
        } else {
            deleted.refused_entry_count += 1;
            tracing::warn!(file.path = %path.display(), "purge.entry_refused");
        }
    }
    Ok(deleted)
}

/// Counts one `NNN-video` folder's files for the log, then removes it. `remove_dir_all` removes
/// a link inside, or the folder swapped for a link, without following it.
fn delete_segment_folder(
    segment_folder: &Path,
    call_ended_at: Timestamp,
    deleted: &mut DeletionCount,
) -> Result<(), FinalizeError> {
    let io_error = |source| FinalizeError::Io {
        path: segment_folder.to_owned(),
        source,
    };
    let mut file_count = 0_u64;
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(segment_folder).map_err(io_error)? {
        let metadata =
            std::fs::symlink_metadata(entry.map_err(io_error)?.path()).map_err(io_error)?;
        file_count += 1;
        if metadata.file_type().is_file() {
            bytes += metadata.len();
        }
    }
    std::fs::remove_dir_all(segment_folder).map_err(io_error)?;
    deleted.file_count += file_count;
    deleted.bytes += bytes;
    tracing::info!(
        file.directory = %segment_folder.display(),
        purge.deleted_file_count = file_count,
        purge.deleted_bytes = bytes,
        purge.call_ended_at = %call_ended_at,
        "purge.segment_folder_deleted"
    );
    Ok(())
}
