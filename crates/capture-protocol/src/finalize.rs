//! What `mc-capture finalize` and `mc-capture pending-finalize` print on stdout: one JSON object
//! each. The app parses them with these types, so the two sides cannot drift.

use std::path::PathBuf;

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
}

/// The call folders whose `call.mp4` is missing or older than their manifests, and whose last
/// finalize did not fail on the same manifests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingFinalize {
    pub folders: Vec<PathBuf>,
}
