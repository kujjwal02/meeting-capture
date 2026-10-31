//! A call's video: one window recorded through ScreenCaptureKit, encoded in hardware by
//! VideoToolbox, and cut by AVAssetWriter into 2 s HLS fragments in numbered segments of the call
//! folder, with `video.json`. Every frame carries mach host time, as the audio stems do, so
//! finalize lines the two up.
//!
//! The manifest, the segment writer and repair are portable, so the app repairs video folders on
//! every OS. Capture is macOS only, in `mac`. Unsafe code lives only in `mac::stream`,
//! `mac::encoder` and `mac::muxer`, which define the Objective-C classes the frameworks call back.

use std::path::{Path, PathBuf};
use std::time::Duration;

mod manifest;
mod playlist;
mod repair;
mod window_pick;
mod writer;

#[cfg(target_os = "macos")]
pub mod mac;

pub use manifest::{
    ClockRecord, FragmentRecord, FrameCounts, MANIFEST_FILE_NAME, MANIFEST_SCHEMA_VERSION,
    PartRecord, VideoCodec, VideoGapCause, VideoGapRecord, VideoManifest, VideoRepair,
    VideoSegmentRecord, VideoSegmentState, WindowChangeCause, WindowChangeRecord, WindowRecord,
};
pub use playlist::PLAYLIST_FILE_NAME;
pub use repair::{VideoFolderRepair, repair_folder};
pub use window_pick::{ListedWindow, WindowHint, pick_window};
pub use writer::{FragmentWritten, SegmentStart, VideoSegmentWriter};

/// The video process's lock file in a call folder. A segment holds it for its whole life, and
/// repair takes it before it reads anything.
pub const LOCK_FILE_NAME: &str = "video.lock";

/// The video process closes its segment within this long of a stop arriving. The app kills a
/// process 12 s after it sends stop, so this stays under that.
pub const SEGMENT_CLOSE_LIMIT: Duration = Duration::from_secs(10);

const SEGMENT_FOLDER_SUFFIX: &str = "-video";

#[derive(Debug, thiserror::Error)]
pub enum VideoWriterError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not a valid video manifest: {source}", path.display())]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{} has schema version {found}; this build reads up to {supported}", path.display())]
    ManifestVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("another process holds {} for video", folder.display())]
    FolderBusy { folder: PathBuf },
    #[error("video segment {number} is taken in {}: the highest is {highest}", folder.display())]
    SegmentNumberTaken {
        folder: PathBuf,
        number: u32,
        highest: u32,
    },
    #[error("a fragment arrived before its part's init segment")]
    NoPart,
    #[error("the system clock is outside the range a timestamp holds")]
    ClockOutOfRange,
}

/// The folder of segment `number`, such as `001-video`.
#[must_use]
pub fn segment_folder_name(number: u32) -> String {
    format!("{number:03}{SEGMENT_FOLDER_SUFFIX}")
}

/// The number of a folder named `NNN-video`.
#[must_use]
pub fn segment_number_of_folder(name: &str) -> Option<u32> {
    let digits = name.strip_suffix(SEGMENT_FOLDER_SUFFIX)?;
    let is_number = digits.len() >= 3 && digits.bytes().all(|byte| byte.is_ascii_digit());
    is_number.then(|| digits.parse().ok()).flatten()
}

/// One past the highest video segment number in the folder's `video.json` or its `NNN-video`
/// folders.
///
/// # Errors
///
/// Fails when the folder or `video.json` cannot be read.
pub fn next_segment_number(folder: &Path) -> Result<u32, VideoWriterError> {
    if !folder.exists() {
        return Ok(1);
    }
    let manifest = VideoManifest::load_or_new(folder)?;
    Ok(highest_segment_number(folder, &manifest)? + 1)
}

fn highest_segment_number(
    folder: &Path,
    manifest: &VideoManifest,
) -> Result<u32, VideoWriterError> {
    let listed = manifest
        .segments
        .iter()
        .map(|segment| segment.number)
        .max()
        .unwrap_or(0);
    let on_disk = repair::segment_folders(folder)?
        .keys()
        .copied()
        .max()
        .unwrap_or(0);
    Ok(listed.max(on_disk))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "fragment lengths are a few seconds, far inside u64 nanoseconds"
)]
pub(crate) fn ms_to_ns(ms: f64) -> u64 {
    (ms.max(0.0) * 1e6).round() as u64
}

#[cfg(unix)]
pub(crate) fn sync_folder(folder: &Path) -> Result<(), VideoWriterError> {
    std::fs::File::open(folder)
        .and_then(|handle| handle.sync_all())
        .map_err(|source| VideoWriterError::Io {
            path: folder.to_owned(),
            source,
        })
}

/// Windows cannot open a folder as a file to flush it. NTFS journals the rename itself.
#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "matches the Unix version's signature"
)]
pub(crate) fn sync_folder(_folder: &Path) -> Result<(), VideoWriterError> {
    Ok(())
}
