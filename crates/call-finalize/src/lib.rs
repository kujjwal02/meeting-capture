//! Finalize: after a call, one `call.mp4` in its call folder, with the video segments as one
//! track and the mic and remote stems mixed into one AAC track, each at its place in time. The
//! WAV stems and the video fragments stay as they are.
//!
//! Placing and mixing are portable, in `plan` and `mix`, so the Windows capture can reuse them.
//! Writing the MP4 is macOS only, in `mac`, through AVFoundation: the video passes through as it
//! was encoded, and only the mix is encoded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;

use call_folder::FolderLock;
pub use capture_protocol::finalize::{FinalizeOutcome, Mp4Summary, PendingFinalize};

#[cfg(target_os = "macos")]
mod mac;
pub mod mix;
pub mod plan;
mod record;

pub use plan::{
    CallTimeline, InputTrack, PartStart, PlacedPart, PlacedStem, SkipReason, SkippedInput,
    StemChunk, VideoPartInput, plan, plan_folder,
};
pub use record::{FINALIZE_RECORD_FILE_NAME, needs_finalize};

/// The call's playable file, in its call folder.
pub const OUTPUT_FILE_NAME: &str = "call.mp4";

/// Held while finalize works, so the app's call-end run and its launch run never write the same
/// file at once.
const LOCK_FILE_NAME: &str = "finalize.lock";

/// How long a run waits for another to let go of the folder. A two-hour call finalizes in about
/// two minutes, so a longer hold means that run hangs.
const LOCK_WAIT: Duration = Duration::from_secs(300);

/// The file being written, renamed to `call.mp4` once complete.
const PARTIAL_FILE_NAME: &str = ".call.partial.mp4";

/// Each video part joined into one fragmented MP4 while it is read: `.finalize-SSS-P.mp4`.
const JOINED_PART_PREFIX: &str = ".finalize-";

/// The mix as AAC. The stems are 16 kHz mono, which carries speech up to 8 kHz; AAC-LC at this
/// rate tops out near 48 kbps, so a higher rate would buy nothing.
pub const MIX_BIT_RATE_BPS: u32 = 48_000;

#[derive(Debug, thiserror::Error)]
pub enum FinalizeError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Audio(#[from] stem_writer::StemWriterError),
    #[error(transparent)]
    Video(#[from] video_capture::VideoWriterError),
    #[error("could not write {}: {message}", path.display())]
    Mp4Write { path: PathBuf, message: String },
    #[error("{} could not be read as video: {message}", path.display())]
    PartRead { path: PathBuf, message: String },
    #[error("{} is not a finalize record: {source}", path.display())]
    Record {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("call.mp4 is written through AVFoundation, which only macOS has")]
    Unsupported,
}

/// Repairs what a dead writer left open, then writes `call.mp4` from every closed or recovered
/// segment. The previous `call.mp4`, if any, stays until the new one is complete. Each run that
/// reaches the media writes the folder's finalize record, a failed one too.
///
/// # Errors
///
/// Fails only when the folder lock or the finalize record cannot be read or written. A failure
/// to read the media or write the file is the outcome `Failed`.
pub fn finalize_folder(folder: &Path) -> Result<FinalizeOutcome, FinalizeError> {
    let lock = FolderLock::take_within(folder, LOCK_FILE_NAME, LOCK_WAIT).map_err(|source| {
        FinalizeError::Io {
            path: folder.join(LOCK_FILE_NAME),
            source,
        }
    })?;
    let Some(_lock) = lock else {
        return Ok(FinalizeOutcome::Busy);
    };
    match finalize_locked(folder) {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            let outcome = FinalizeOutcome::Failed {
                message: error.to_string(),
            };
            // Any manifest that can be stamped gets the failure, so the launch run does not
            // meet the same early error at every launch.
            if let Ok(inputs) = record::InputStamp::read(folder) {
                record::write_finished(folder, &inputs, &outcome)?;
            }
            remove_stray_files(folder)?;
            Ok(outcome)
        }
    }
}

/// The run under the folder lock. An error is the outcome `Failed`, recorded by the caller.
fn finalize_locked(folder: &Path) -> Result<FinalizeOutcome, FinalizeError> {
    remove_stray_files(folder)?;
    if !repair(folder)? {
        return Ok(FinalizeOutcome::Recording);
    }
    // After repair, which rewrites the manifests it fixes.
    let inputs = record::InputStamp::read(folder)?;
    record::write_started(folder, &inputs)?;
    let outcome = plan_and_write(folder)?;
    record::write_finished(folder, &inputs, &outcome)?;
    Ok(outcome)
}

/// False while a capture process holds the folder.
fn repair(folder: &Path) -> Result<bool, FinalizeError> {
    let audio_repair = stem_writer::repair_folder(folder)?;
    let video_repair = video_capture::repair_folder(folder)?;
    Ok(!matches!(audio_repair, stem_writer::FolderRepair::Busy)
        && !matches!(video_repair, video_capture::VideoFolderRepair::Busy))
}

fn plan_and_write(folder: &Path) -> Result<FinalizeOutcome, FinalizeError> {
    let timeline = plan_folder(folder)?;
    let Some(started_at) = timeline.started_at.filter(|_| !timeline.parts.is_empty()) else {
        return Ok(FinalizeOutcome::NoVideo {
            skipped: timeline.skipped,
        });
    };
    write_mp4(folder, &timeline, started_at)
}

/// What a run killed mid-write left: the partial file and joined parts.
fn remove_stray_files(folder: &Path) -> Result<(), FinalizeError> {
    let io_error = |source| FinalizeError::Io {
        path: folder.to_owned(),
        source,
    };
    for entry in std::fs::read_dir(folder).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_stray = name == PARTIAL_FILE_NAME
            || (name.starts_with(JOINED_PART_PREFIX) && name.ends_with(".mp4"));
        if is_stray {
            remove_if_present(&entry.path())?;
        }
    }
    Ok(())
}

pub(crate) fn remove_if_present(path: &Path) -> Result<(), FinalizeError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(FinalizeError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

#[cfg(target_os = "macos")]
fn write_mp4(
    folder: &Path,
    timeline: &CallTimeline,
    started_at: Timestamp,
) -> Result<FinalizeOutcome, FinalizeError> {
    let output = folder.join(OUTPUT_FILE_NAME);
    let written = mac::write_call_mp4(folder, &output, timeline)?;
    #[expect(
        clippy::cast_precision_loss,
        reason = "call lengths stay far below 2^52 ns"
    )]
    let length_ms = written.length_ns as f64 / 1e6;
    Ok(FinalizeOutcome::Written(Mp4Summary {
        file: output,
        started_at,
        length_ms,
        video_part_count: timeline.parts.len(),
        stem_count: timeline.stems.len(),
        video_frame_count: written.video_frame_count,
        parts_shifted: written.parts_shifted,
        parts_reencoded: written.parts_reencoded,
        skipped: timeline.skipped.clone(),
    }))
}

#[cfg(not(target_os = "macos"))]
fn write_mp4(
    _folder: &Path,
    _timeline: &CallTimeline,
    _started_at: Timestamp,
) -> Result<FinalizeOutcome, FinalizeError> {
    Err(FinalizeError::Unsupported)
}
