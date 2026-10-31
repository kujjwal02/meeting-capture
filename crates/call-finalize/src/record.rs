//! `finalize.json`: what the last finalize of a call folder read and how it ended. The launch
//! run compares it with the manifests now, so a folder is finalized again only when a manifest
//! changed, and a folder that failed or hung is not retried at every launch.

use std::io::Write as _;
use std::path::Path;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    FinalizeError, FinalizeOutcome, InputTrack, OUTPUT_FILE_NAME, SkipReason, SkippedInput,
};

pub const FINALIZE_RECORD_FILE_NAME: &str = "finalize.json";

/// Raised when the layout changes in a way an older reader would misread. A reader that meets
/// another version finalizes the folder again and writes its own. Version 1 had no skip list.
const RECORD_SCHEMA_VERSION: u32 = 2;

/// The first version whose finished runs list the video they skipped.
const SKIP_LIST_SCHEMA_VERSION: u32 = 2;

/// A run that failed or never finished, killed by the app's time limit or a quit, gets one more
/// try on the same manifests, at the next launch.
const MAX_ATTEMPTS: u32 = 2;

/// The manifests a run planned from, as the file system reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InputStamp {
    audio_manifest: Option<FileStamp>,
    video_manifest: Option<FileStamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    modified_at: Timestamp,
    bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct FinalizeRecord {
    schema_version: u32,
    inputs: InputStamp,
    /// Runs on these same inputs, this one included.
    attempt_count: u32,
    run: RecordedRun,
    recorded_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum RecordedRun {
    /// Planning or writing. Still there after the run means it was killed.
    Started,
    Written {
        skipped_video: Vec<SkippedVideo>,
    },
    NoVideo {
        skipped_video: Vec<SkippedVideo>,
    },
    Failed {
        message: String,
    },
}

/// A video segment, or a part of one, that the run left out of `call.mp4`, and why. The purge
/// keeps the segment's folder unless the reason is `Empty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SkippedVideo {
    pub(crate) segment: u32,
    pub(crate) reason: SkipReason,
}

impl InputStamp {
    pub(crate) fn read(folder: &Path) -> Result<Self, FinalizeError> {
        Ok(Self {
            audio_manifest: file_stamp(&folder.join(stem_writer::MANIFEST_FILE_NAME))?,
            video_manifest: file_stamp(&folder.join(video_capture::MANIFEST_FILE_NAME))?,
        })
    }
}

fn file_stamp(path: &Path) -> Result<Option<FileStamp>, FinalizeError> {
    let io_error = |source| FinalizeError::Io {
        path: path.to_owned(),
        source,
    };
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(io_error(source)),
    };
    let modified = metadata.modified().map_err(io_error)?;
    let modified_at =
        Timestamp::try_from(modified).map_err(|error| io_error(std::io::Error::other(error)))?;
    Ok(Some(FileStamp {
        modified_at,
        bytes: metadata.len(),
    }))
}

/// Where the folder's finalize stands against its manifests now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FinalizeStanding {
    /// `call.mp4` is written from the current manifests, or they hold no video to place.
    Succeeded,
    /// A run is due: none yet, a manifest changed since, `call.mp4` is gone, or a failed or
    /// killed run has a retry left.
    Pending,
    /// Runs on the current manifests failed or were killed with no retry left, or there is no
    /// `video.json` to finalize from.
    Failed,
}

pub(crate) fn finalize_standing(folder: &Path) -> FinalizeStanding {
    if !folder.join(video_capture::MANIFEST_FILE_NAME).exists() {
        return FinalizeStanding::Failed;
    }
    let Ok(inputs) = InputStamp::read(folder) else {
        return FinalizeStanding::Pending;
    };
    let Some(record) = read_record(folder) else {
        return FinalizeStanding::Pending;
    };
    if record.inputs != inputs {
        return FinalizeStanding::Pending;
    }
    match record.run {
        RecordedRun::Written { .. } if folder.join(OUTPUT_FILE_NAME).exists() => {
            FinalizeStanding::Succeeded
        }
        RecordedRun::Written { .. } => FinalizeStanding::Pending,
        RecordedRun::NoVideo { .. } => FinalizeStanding::Succeeded,
        RecordedRun::Started | RecordedRun::Failed { .. }
            if record.attempt_count < MAX_ATTEMPTS =>
        {
            FinalizeStanding::Pending
        }
        RecordedRun::Started | RecordedRun::Failed { .. } => FinalizeStanding::Failed,
    }
}

/// The video segments the last finished run left out of `call.mp4`. `None` when the record is
/// missing, unreadable, of another schema version, or not of a finished run. Version 1 had no
/// skip list, so its finalize state counts as unknown.
pub(crate) fn recorded_skipped_video(folder: &Path) -> Option<Vec<SkippedVideo>> {
    let record =
        read_record(folder).filter(|record| record.schema_version == SKIP_LIST_SCHEMA_VERSION)?;
    match record.run {
        RecordedRun::Written { skipped_video } | RecordedRun::NoVideo { skipped_video } => {
            Some(skipped_video)
        }
        RecordedRun::Started | RecordedRun::Failed { .. } => None,
    }
}

/// Whether the folder has video that `call.mp4` does not hold yet, and no finished or failed
/// run on the same manifests. A folder whose video the purge deleted has none to finalize.
#[must_use]
pub fn needs_finalize(folder: &Path) -> bool {
    if !folder.join(video_capture::MANIFEST_FILE_NAME).exists()
        || crate::purge::has_purge_record(folder)
    {
        return false;
    }
    finalize_standing(folder) == FinalizeStanding::Pending
}

/// Marks the run begun before any media is read, so a run killed in the middle still counts.
pub(crate) fn write_started(folder: &Path, inputs: &InputStamp) -> Result<(), FinalizeError> {
    let attempt_count = read_record(folder)
        .filter(|record| record.inputs == *inputs)
        .map_or(1, |record| record.attempt_count + 1);
    write_record(folder, *inputs, attempt_count, RecordedRun::Started)
}

/// Busy, Recording and Purged never reach here: those runs read no media.
pub(crate) fn write_finished(
    folder: &Path,
    inputs: &InputStamp,
    outcome: &FinalizeOutcome,
) -> Result<(), FinalizeError> {
    let run = match outcome {
        FinalizeOutcome::Busy | FinalizeOutcome::Recording | FinalizeOutcome::Purged => {
            return Ok(());
        }
        FinalizeOutcome::NoVideo { skipped } => RecordedRun::NoVideo {
            skipped_video: skipped_video(skipped),
        },
        FinalizeOutcome::Written(summary) => RecordedRun::Written {
            skipped_video: skipped_video(&summary.skipped),
        },
        FinalizeOutcome::Failed { message } => RecordedRun::Failed {
            message: message.clone(),
        },
    };
    // A run that failed before it marked itself started is an attempt of its own.
    let attempt_count = match read_record(folder).filter(|record| record.inputs == *inputs) {
        Some(record) if matches!(record.run, RecordedRun::Started) => record.attempt_count,
        Some(record) => record.attempt_count + 1,
        None => 1,
    };
    write_record(folder, *inputs, attempt_count, run)
}

fn skipped_video(skipped: &[SkippedInput]) -> Vec<SkippedVideo> {
    skipped
        .iter()
        .filter(|input| input.track == InputTrack::Video)
        .map(|input| SkippedVideo {
            segment: input.segment,
            reason: input.reason,
        })
        .collect()
}

fn read_record(folder: &Path) -> Option<FinalizeRecord> {
    let text = std::fs::read_to_string(folder.join(FINALIZE_RECORD_FILE_NAME)).ok()?;
    serde_json::from_str::<FinalizeRecord>(&text)
        .ok()
        .filter(|record| record.schema_version == RECORD_SCHEMA_VERSION)
}

/// Written beside, then renamed, so a reader never sees half a record.
fn write_record(
    folder: &Path,
    inputs: InputStamp,
    attempt_count: u32,
    run: RecordedRun,
) -> Result<(), FinalizeError> {
    let path = folder.join(FINALIZE_RECORD_FILE_NAME);
    let record = FinalizeRecord {
        schema_version: RECORD_SCHEMA_VERSION,
        inputs,
        attempt_count,
        run,
        recorded_at: Timestamp::now(),
    };
    let text = serde_json::to_string_pretty(&record).map_err(|source| FinalizeError::Record {
        path: path.clone(),
        source,
    })?;
    let partial = folder.join(format!(".{FINALIZE_RECORD_FILE_NAME}.partial"));
    let io_error = |source| FinalizeError::Io {
        path: path.clone(),
        source,
    };
    let mut file = std::fs::File::create(&partial).map_err(io_error)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io_error)?;
    std::fs::rename(&partial, &path).map_err(io_error)
}
