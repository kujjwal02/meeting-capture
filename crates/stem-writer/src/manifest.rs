//! `audio.json`: the audio process's record of a call folder. Each segment lists its stems and how
//! their files map to host time and UTC. The video process keeps its own manifest; finalize reads
//! both.

use std::io::Write;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sources::Platform;
use stem_timing::{Stem, StemLosses};

use crate::{OUTPUT_RATE_HZ, StemWriterError};

pub const MANIFEST_FILE_NAME: &str = "audio.json";

/// Raised when the layout changes in a way an older reader would misread. Readers accept every
/// version up to their own; fields added since carry serde defaults.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioManifest {
    pub schema_version: u32,
    pub segments: Vec<SegmentRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentState {
    /// Being written, or the writer died before closing it.
    Open,
    /// The writer finished it: headers final, every sample on disk.
    Closed,
    /// The writer died before closing it. Repair truncated each file to a whole sample and
    /// rebuilt its header from the file length.
    Recovered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentRecord {
    /// The `NNN` in the segment's file names.
    pub number: u32,
    pub call_id: String,
    pub platform: Platform,
    pub state: SegmentState,
    /// The host clock read against UTC when the segment started. Every UTC time in the segment
    /// comes from this reading.
    pub clock: ClockRecord,
    /// A second reading at close. Its offset from `clock` shows how far the two clocks drifted.
    #[serde(default)]
    pub closing_clock: Option<ClockRecord>,
    #[serde(default)]
    pub closed_at: Option<Timestamp>,
    #[serde(default)]
    pub stems: Vec<StemRecord>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClockRecord {
    pub host_time_ns: u64,
    pub utc: Timestamp,
    /// The reading's error bound.
    pub read_gap_ns: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StemRecord {
    pub stem: Stem,
    pub file: String,
    /// Host time of the file's first sample. `None` until the first stamped buffer arrives.
    #[serde(default)]
    pub first_host_time_ns: Option<u64>,
    /// `first_host_time_ns` in UTC, through the segment's clock reading.
    #[serde(default)]
    pub started_at: Option<Timestamp>,
    /// Host time at the end of `samples_written`.
    #[serde(default)]
    pub last_host_time_ns: Option<u64>,
    /// The file's rate. Always 16 kHz.
    pub sample_rate_hz: u32,
    /// Samples the header counts: the snapshot at the last header sync.
    pub samples_written: u64,
    /// Each input device, from the file position where it began.
    #[serde(default)]
    pub devices: Vec<DeviceRecord>,
    /// Each delivered input rate, from the file position where it began.
    #[serde(default)]
    pub rates_in: Vec<RateRecord>,
    #[serde(default)]
    pub gaps: Vec<GapRecord>,
    #[serde(default)]
    pub runs_ahead: Vec<AheadRecord>,
    /// The capture ring's own loss counters.
    #[serde(default)]
    pub ring_losses: StemLosses,
    /// Why the writer stopped this stem early, when it did. The file holds the audio before.
    #[serde(default)]
    pub stopped_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub start_ms: f64,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RateRecord {
    pub start_ms: f64,
    pub rate_hz: u32,
}

/// Silence the writer inserted, so later audio stays at its host time. The silence spans the
/// host time it stands for.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GapRecord {
    /// Position in the file where the silence starts.
    pub start_ms: f64,
    /// Host time the silence starts at.
    pub start_host_time_ns: u64,
    pub length_ms: f64,
    pub cause: GapCause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapCause {
    /// The capture ring was full, so the tail of a buffer never reached the writer. The silence
    /// has the exact length lost.
    RingOverflow,
    /// A buffer's host time jumped more than 70 ms past the end of the audio before it: the
    /// device delivered nothing for that span.
    NoAudioDelivered,
    /// The stem moved to another device, which started this long after the last one stopped.
    DeviceSwitch,
}

impl GapCause {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RingOverflow => "ring_overflow",
            Self::NoAudioDelivered => "no_audio_delivered",
            Self::DeviceSwitch => "device_switch",
        }
    }
}

impl std::fmt::Display for GapCause {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A span where buffers arrived with host times more than 70 ms behind the audio already in the
/// file, as when a new device's first buffer overlaps the old one's last. The writer keeps that
/// audio, since dropping it would lose speech. Clock steering then takes up to 1 ms per second
/// of audio until the file meets host time again. A gap inside the span takes up what lead is
/// left, and a rotation carries it into the next file as a span at 0 ms.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AheadRecord {
    pub start_ms: f64,
    pub start_host_time_ns: u64,
    /// How far the file ran ahead of host time at `start_ms`.
    pub start_ahead_ms: f64,
    pub max_ahead_ms: f64,
    /// Where the span ended. `None` while it still runs ahead.
    #[serde(default)]
    pub end_ms: Option<f64>,
    /// The lead left at `end_ms`: 0 when steering closed the span, more when a gap or a
    /// rotation ended it.
    #[serde(default)]
    pub end_ahead_ms: f64,
}

impl StemRecord {
    /// Host time of a position in the file. Gaps need no correction: their silence spans the
    /// host time it stands for. Inside a run ahead, the file leads host time by `start_ahead_ms`
    /// at its start, changing linearly to `end_ahead_ms` at `end_ms` as steering pulls it back.
    #[must_use]
    pub fn host_time_at(&self, file_ms: f64) -> Option<u64> {
        let first_ns = self.first_host_time_ns?;
        let mut host_ms = file_ms;
        for span in &self.runs_ahead {
            if file_ms < span.start_ms {
                continue;
            }
            let ahead_ms = match span.end_ms {
                Some(end_ms) if file_ms >= end_ms => 0.0,
                Some(end_ms) => {
                    let progress = (file_ms - span.start_ms) / (end_ms - span.start_ms).max(1e-9);
                    span.start_ahead_ms + (span.end_ahead_ms - span.start_ahead_ms) * progress
                }
                None => span.start_ahead_ms,
            };
            host_ms -= ahead_ms;
        }
        let offset_ns = f64_ns(host_ms * 1e6)?;
        first_ns.checked_add_signed(offset_ns)
    }

    /// File length in milliseconds, as the header counts it.
    #[must_use]
    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts stay far below 2^52"
    )]
    pub fn duration_ms(&self) -> f64 {
        self.samples_written as f64 * 1e3 / f64::from(OUTPUT_RATE_HZ)
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "checked against the i64 range first"
)]
fn f64_ns(nanos: f64) -> Option<i64> {
    let in_range = nanos.is_finite() && nanos.abs() < 9.0e18;
    in_range.then(|| nanos.round() as i64)
}

impl AudioManifest {
    /// Reads the folder's `audio.json`. Accepts every schema version up to this build's.
    ///
    /// # Errors
    ///
    /// Fails when the file is missing or unreadable, is not a manifest, or comes from a newer
    /// schema.
    pub fn load(folder: &Path) -> Result<Self, StemWriterError> {
        let path = folder.join(MANIFEST_FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(path, &text),
            Err(source) => Err(StemWriterError::Io { path, source }),
        }
    }

    /// Reads the folder's manifest, or starts an empty one when the folder has none.
    pub(crate) fn load_or_new(folder: &Path) -> Result<Self, StemWriterError> {
        let path = folder.join(MANIFEST_FILE_NAME);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(path, &text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                schema_version: MANIFEST_SCHEMA_VERSION,
                segments: Vec::new(),
            }),
            Err(source) => Err(StemWriterError::Io { path, source }),
        }
    }

    fn parse(path: PathBuf, text: &str) -> Result<Self, StemWriterError> {
        let mut manifest: Self =
            serde_json::from_str(text).map_err(|source| StemWriterError::Manifest {
                path: path.clone(),
                source,
            })?;
        if manifest.schema_version > MANIFEST_SCHEMA_VERSION {
            return Err(StemWriterError::ManifestVersion {
                path,
                found: manifest.schema_version,
                supported: MANIFEST_SCHEMA_VERSION,
            });
        }
        // The next store writes every field of the current version.
        manifest.schema_version = MANIFEST_SCHEMA_VERSION;
        Ok(manifest)
    }

    /// Replaces the folder's manifest in one step: a temp file in the same folder, fsync,
    /// rename, then fsync of the folder so the rename itself survives a power cut.
    pub(crate) fn store(&self, folder: &Path) -> Result<(), StemWriterError> {
        let path = folder.join(MANIFEST_FILE_NAME);
        let io_error = |path: PathBuf| move |source| StemWriterError::Io { path, source };
        let json = serde_json::to_vec_pretty(self).map_err(|source| StemWriterError::Manifest {
            path: path.clone(),
            source,
        })?;
        let mut staged = tempfile::Builder::new()
            .prefix(".audio.")
            .suffix(".json")
            .tempfile_in(folder)
            .map_err(io_error(folder.to_owned()))?;
        staged
            .write_all(&json)
            .and_then(|()| staged.as_file().sync_all())
            .map_err(io_error(staged.path().to_owned()))?;
        staged.persist(&path).map_err(|error| StemWriterError::Io {
            path: path.clone(),
            source: error.error,
        })?;
        sync_folder(folder)
    }
}

#[cfg(unix)]
fn sync_folder(folder: &Path) -> Result<(), StemWriterError> {
    std::fs::File::open(folder)
        .and_then(|handle| handle.sync_all())
        .map_err(|source| StemWriterError::Io {
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
fn sync_folder(_folder: &Path) -> Result<(), StemWriterError> {
    Ok(())
}
