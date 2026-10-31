//! What `mc-capture finalize`, `mc-capture pending-finalize` and `mc-capture purge-media` print
//! on stdout: one JSON object each. The app parses them with these types, so the two sides
//! cannot drift.

use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// What one `finalize --folder` run did.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FinalizeOutcome {
    /// Another finalize held the folder for the whole wait.
    Busy,
    /// A capture process records in the folder now. Its call has not ended.
    Recording,
    /// No video segment could be placed, so there is no `call.mp4` to write.
    NoVideo {
        skipped: Vec<SkippedInput>,
    },
    Written(Mp4Summary),
    /// A manifest, a stem or a fragment could not be read, or AVFoundation refused the file.
    /// The folder's finalize record keeps the message, so the launch run does not retry it
    /// until a manifest changes.
    Failed {
        message: String,
    },
    /// The purge deleted the folder's video, so there is nothing to finalize.
    Purged,
}

impl FinalizeOutcome {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Recording => "recording",
            Self::NoVideo { .. } => "no_video",
            Self::Written(_) => "written",
            Self::Failed { .. } => "failed",
            Self::Purged => "purged",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mp4Summary {
    pub file: PathBuf,
    /// UTC at the file's 0 s.
    pub started_at: Timestamp,
    pub length_ms: f64,
    pub video_part_count: usize,
    pub stem_count: usize,
    pub video_frame_count: u64,
    /// Parts whose clock reading put their first frame at or before the previous part's last,
    /// moved to one frame after it so the track's times only rise.
    pub parts_shifted: u64,
    /// Parts whose codec or size differed from the first part's, decoded and encoded again in
    /// its codec at its size, letterboxed. The rest are copied as they are.
    pub parts_reencoded: u64,
    pub skipped: Vec<SkippedInput>,
}

/// A track of one segment that `call.mp4` leaves out.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SkippedInput {
    pub segment: u32,
    pub track: InputTrack,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTrack {
    Video,
    Mic,
    Remote,
}

impl From<stem_timing::Stem> for InputTrack {
    fn from(stem: stem_timing::Stem) -> Self {
        match stem {
            stem_timing::Stem::Mic => Self::Mic,
            stem_timing::Stem::Remote => Self::Remote,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A writer still records it, or died and repair has not run yet.
    StillOpen,
    /// Repair rebuilt it from its files alone, so no clock reading places it.
    NoClock,
    /// It holds no frame or no sample.
    Empty,
    /// A fragment names a part the segment has no record of, so no init segment decodes it.
    NoPartRecord,
}

/// The call folders whose `call.mp4` is missing or older than their manifests, and whose last
/// finalize did not fail on the same manifests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingFinalize {
    pub folders: Vec<PathBuf>,
}

/// How often the app runs the purge.
pub const MEDIA_PURGE_INTERVAL: Duration = Duration::from_hours(24);

/// What one `mc-capture purge-media --calls-dir` run did. The helper also logs each deletion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPurge {
    /// The purge's own clock, which the run aged every call against. It follows the wall clock
    /// back at once and forward by at most two days a run. A call must be 90 days and two days
    /// old on it. `None` when there is no calls folder.
    pub trusted_now: Option<Timestamp>,
    /// Call folders with video whose first-seen stamp this run wrote: seen for the first time,
    /// or moved up to a purge clock started again after its record was lost.
    pub first_seen_count: u64,
    pub purged: Vec<PurgedCall>,
    /// Call folders past the age limit that still hold video, and why it stayed.
    pub kept: Vec<KeptCall>,
}

/// A call folder whose `call.mp4` and video fragments the run deleted. Its stems stay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PurgedCall {
    pub folder: PathBuf,
    /// The newest modified time of a capture file: a stem, a manifest or a video fragment.
    pub call_ended_at: Timestamp,
    /// When a purge run first saw the folder, on the purge's own clock. The call's age counts
    /// from the later of this and `call_ended_at`.
    pub first_seen_at: Timestamp,
    pub deleted_file_count: u64,
    pub deleted_bytes: u64,
    /// Symbolic links and other entries the run would not delete through.
    pub refused_entry_count: u64,
    /// `NNN-video` folders left in place because `call.mp4` does not hold their frames.
    pub unplaced_segments: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeptCall {
    pub folder: PathBuf,
    pub reason: KeepReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KeepReason {
    /// A capture process holds `audio.lock` or `video.lock`.
    Recording,
    /// A finalize run holds `finalize.lock`.
    Finalizing,
    /// The folder still waits for a `call.mp4`: no finalize record, manifests changed since the
    /// last run, or a failed run with a retry left.
    FinalizePending,
    /// Finalize failed or was killed on the current manifests with no retry left. Also set when
    /// the folder holds `call.mp4` or `NNN-video` folders but no `video.json`, so finalize has
    /// nothing to plan from, or when a purged folder's finalize record is unreadable. Fragments
    /// stay until a finalize succeeds.
    FinalizeFailed,
    /// Every video entry left is an `NNN-video` folder whose frames `call.mp4` does not hold:
    /// finalize skipped it with no clock to place it, or `video.json` does not list it.
    UnplacedVideo {
        segments: Vec<u32>,
    },
    /// `video.json` lists a segment a dead writer left open, which repair has not ended yet.
    OpenVideoSegment,
    /// The folder resolves to a place outside the calls folder.
    OutsideRoot,
    Failed {
        message: String,
    },
}

impl KeepReason {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Finalizing => "finalizing",
            Self::FinalizePending => "finalize_pending",
            Self::FinalizeFailed => "finalize_failed",
            Self::UnplacedVideo { .. } => "unplaced_video",
            Self::OpenVideoSegment => "open_video_segment",
            Self::OutsideRoot => "outside_root",
            Self::Failed { .. } => "failed",
        }
    }
}
