//! `video.json`: the video process's record of a call folder. Each segment lists its fragments
//! and how they map to host time and UTC, the gaps the writer filled by repeating the last frame,
//! and each window change. `audio.json` is the audio process's; finalize reads both.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sources::Platform;

use crate::VideoWriterError;

pub const MANIFEST_FILE_NAME: &str = "video.json";

/// Raised when the layout changes in a way an older reader would misread. Readers accept every
/// version up to their own; fields added since carry serde defaults.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoManifest {
    pub schema_version: u32,
    pub segments: Vec<VideoSegmentRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoSegmentState {
    /// Being written, or the writer died before closing it.
    Open,
    /// The writer finished it: every fragment on disk and the playlist ended.
    Closed,
    /// The writer died before closing it. Repair dropped partial fragments and ended the
    /// playlist.
    Recovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    Hevc,
    H264,
}

impl VideoCodec {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hevc => "hevc",
            Self::H264 => "h264",
        }
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoSegmentRecord {
    /// The `NNN` in the segment's folder name. It matches the audio segment of the same span
    /// until a video restart takes the next free number; `audio_segment` says which pairs.
    pub number: u32,
    /// The audio segment that was open when this segment started. `None` only in a segment
    /// repair rebuilt from its files alone.
    #[serde(default)]
    pub audio_segment: Option<u32>,
    /// `None` only in a segment repair rebuilt from its files alone.
    #[serde(default)]
    pub call_id: Option<String>,
    pub platform: Platform,
    pub state: VideoSegmentState,
    /// The segment's folder in the call folder, such as `001-video`.
    pub folder: String,
    /// The HLS playlist in `folder` that lists every fragment in order.
    pub playlist: String,
    /// The first part's codec. Each part records its own, since a new part may fall back.
    pub codec: VideoCodec,
    /// The host clock read against UTC when the segment started. `None` only in a segment
    /// repair rebuilt from its files alone.
    #[serde(default)]
    pub clock: Option<ClockRecord>,
    /// A second reading at close, to show drift between the two clocks.
    #[serde(default)]
    pub closing_clock: Option<ClockRecord>,
    #[serde(default)]
    pub closed_at: Option<Timestamp>,
    /// The window the segment started on. Later windows are in `window_changes`.
    #[serde(default)]
    pub window: Option<WindowRecord>,
    /// Host time of the first frame. `None` until a frame arrives.
    #[serde(default)]
    pub first_host_time_ns: Option<u64>,
    /// `first_host_time_ns` in UTC, through `clock`.
    #[serde(default)]
    pub started_at: Option<Timestamp>,
    /// Host time at the end of the last fragment on disk.
    #[serde(default)]
    pub last_host_time_ns: Option<u64>,
    /// Each run of fragments that share one init segment. A window that changed its shape for
    /// good starts the next part at the new size.
    #[serde(default)]
    pub parts: Vec<PartRecord>,
    #[serde(default)]
    pub fragments: Vec<FragmentRecord>,
    #[serde(default)]
    pub gaps: Vec<VideoGapRecord>,
    #[serde(default)]
    pub window_changes: Vec<WindowChangeRecord>,
    #[serde(default)]
    pub frames: FrameCounts,
    /// Streams rebuilt after they stopped or delivered nothing for 5 s.
    #[serde(default)]
    pub stream_rebuilds: u32,
    /// Why the writer ended the segment early, when it did.
    #[serde(default)]
    pub stopped_by: Option<String>,
    #[serde(default)]
    pub repair: Option<VideoRepair>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClockRecord {
    pub host_time_ns: u64,
    pub utc: Timestamp,
    /// The reading's error bound.
    pub read_gap_ns: u64,
}

/// A window as ScreenCaptureKit lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRecord {
    pub window_id: u32,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub pid: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartRecord {
    /// 1 for the segment's first part.
    pub number: u32,
    /// The part's init segment in the segment folder, such as `init-1.mp4`.
    pub init: String,
    pub codec: VideoCodec,
    pub width_px: u32,
    pub height_px: u32,
    /// Host time of the part's first frame.
    pub first_host_time_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FragmentRecord {
    /// The fragment in the segment folder, such as `00001.m4s`.
    pub file: String,
    /// The part whose init segment decodes it.
    pub part: u32,
    /// Host time of its first frame. `None` only for a fragment repair found on disk unlisted.
    #[serde(default)]
    pub first_host_time_ns: Option<u64>,
    pub duration_ms: f64,
    pub bytes: u64,
}

/// A span with no new frame, which the writer filled by repeating the last one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct VideoGapRecord {
    pub start_host_time_ns: u64,
    pub length_ms: f64,
    pub cause: VideoGapCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoGapCause {
    /// The window was minimised, on another Space, or the screen was locked.
    WindowHidden,
    /// No window matched the hint: it closed, or has not reopened yet.
    WindowMissing,
    /// The stream stopped or stalled, and was being rebuilt.
    StreamRestart,
    /// The process stood still for over 1 s, as across a sleep it was not stopped for. The part
    /// ended at the last frame before it, so no frame stretches over the span.
    ProcessStalled,
}

impl VideoGapCause {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WindowHidden => "window_hidden",
            Self::WindowMissing => "window_missing",
            Self::StreamRestart => "stream_restart",
            Self::ProcessStalled => "process_stalled",
        }
    }
}

impl std::fmt::Display for VideoGapCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowChangeRecord {
    pub host_time_ns: u64,
    pub cause: WindowChangeCause,
    /// The window recorded from now on. `None` once it closed with no match left.
    #[serde(default)]
    pub window: Option<WindowRecord>,
    /// The window's size in points, as its frames report it.
    #[serde(default)]
    pub content_width_pt: Option<f64>,
    #[serde(default)]
    pub content_height_pt: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowChangeCause {
    /// The window's size changed.
    Resized,
    /// The window closed. The process looks for a match every second.
    Closed,
    /// Another window matched the hint after the first one closed, as a huddle window that the
    /// user closed and opened again.
    Replaced,
    /// The window closed and came back with the same id.
    Returned,
}

impl WindowChangeCause {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resized => "resized",
            Self::Closed => "closed",
            Self::Replaced => "replaced",
            Self::Returned => "returned",
        }
    }
}

impl std::fmt::Display for WindowChangeCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCounts {
    /// Complete frames the stream delivered.
    pub captured: u64,
    /// Frames the stream marked idle: the window showed nothing new.
    #[serde(default)]
    pub idle: u64,
    /// Frames handed to the encoder, repeats included.
    pub encoded: u64,
    /// Frames VideoToolbox handed back compressed.
    #[serde(default)]
    pub output: u64,
    /// Repeats of the last frame, while no new one came.
    pub repeated: u64,
    /// Frames the encoder or the muxer refused, and the frames after one that waited for the
    /// next keyframe.
    pub dropped: u64,
}

/// What repair did to a segment whose writer died before closing it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct VideoRepair {
    pub repaired_at: Timestamp,
    /// `video.json` did not list the segment, so repair rebuilt its entry from the files alone.
    pub is_rebuilt_from_files: bool,
    /// Fragment files the writer had not finished, deleted.
    pub partial_fragments_dropped: u32,
    /// Finished fragments the playlist listed but `video.json` had not caught up with yet.
    pub fragments_added: u32,
    /// Fragments `video.json` listed whose files are gone.
    pub fragments_missing: u32,
    pub is_endlist_added: bool,
}

impl VideoManifest {
    /// Reads the folder's `video.json`, or starts an empty one when the folder has none.
    ///
    /// # Errors
    ///
    /// Fails when the file is unreadable, is not a manifest, or comes from a newer schema.
    pub fn load_or_new(folder: &Path) -> Result<Self, VideoWriterError> {
        let path = folder.join(MANIFEST_FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(path, &text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                schema_version: MANIFEST_SCHEMA_VERSION,
                segments: Vec::new(),
            }),
            Err(source) => Err(VideoWriterError::Io { path, source }),
        }
    }

    fn parse(path: PathBuf, text: &str) -> Result<Self, VideoWriterError> {
        let mut manifest: Self =
            serde_json::from_str(text).map_err(|source| VideoWriterError::Manifest {
                path: path.clone(),
                source,
            })?;
        if manifest.schema_version > MANIFEST_SCHEMA_VERSION {
            return Err(VideoWriterError::ManifestVersion {
                path,
                found: manifest.schema_version,
                supported: MANIFEST_SCHEMA_VERSION,
            });
        }
        manifest.schema_version = MANIFEST_SCHEMA_VERSION;
        Ok(manifest)
    }

    /// Replaces the folder's manifest in one step: a temp file in the same folder, fsync,
    /// rename, then fsync of the folder so the rename survives a power cut.
    pub(crate) fn store(&self, folder: &Path) -> Result<(), VideoWriterError> {
        let path = folder.join(MANIFEST_FILE_NAME);
        let io_error = |path: PathBuf| move |source| VideoWriterError::Io { path, source };
        let json =
            serde_json::to_vec_pretty(self).map_err(|source| VideoWriterError::Manifest {
                path: path.clone(),
                source,
            })?;
        let mut staged = tempfile::Builder::new()
            .prefix(".video.")
            .suffix(".json")
            .tempfile_in(folder)
            .map_err(io_error(folder.to_owned()))?;
        staged
            .write_all(&json)
            .and_then(|()| staged.as_file().sync_all())
            .map_err(io_error(staged.path().to_owned()))?;
        staged
            .persist(&path)
            .map_err(|error| VideoWriterError::Io {
                path: path.clone(),
                source: error.error,
            })?;
        crate::sync_folder(folder)
    }
}
