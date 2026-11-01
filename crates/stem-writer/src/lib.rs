//! Writes a call's stems as numbered segments in its call folder: `NNN-mic.wav` and
//! `NNN-remote.wav`, 16 kHz mono 16-bit, plus `audio.json`. Each buffer lands at its host time,
//! so the stems line up with each other and with UTC. Knows nothing about the capture crate: the
//! helper hands it stamps and samples.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use jiff::{SignedDuration, Timestamp};
use sources::Platform;
use stem_timing::{BufferStamp, Stem, StemLosses, UtcAnchor};

mod manifest;
mod repair;
mod track;
mod wav;

pub use manifest::{
    AheadRecord, AudioManifest, ClockRecord, DeviceRecord, GapCause, GapRecord, MANIFEST_FILE_NAME,
    MANIFEST_SCHEMA_VERSION, RateRecord, SegmentRecord, SegmentRepair, SegmentState, StemFix,
    StemRecord, StemRepair,
};
pub use repair::{FolderRepair, repair_folder};
pub use track::StemProgress;
pub use wav::{OutputLevel, SILENCE_DBFS};

use call_folder::FolderLock;
use track::StemTrack;

/// The rate of every stem file.
pub const OUTPUT_RATE_HZ: u32 = 16_000;

/// How often the samples, then the headers, are fsynced. After a power cut or a kernel panic a
/// header is at most this stale, and the loss stays under 5 s once the caller's drain step and
/// the resampler's buffering are added. After a kill -9 the page cache still holds every
/// written sample, and repair rebuilds the header from the file length.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_millis(4_500);

/// A WAV's sizes are u32 byte counts, so a 16 kHz 16-bit file holds at most 37.3 hours. The
/// writer moves to the next segment at 24 hours, far from that limit and under 2.8 GB a file.
pub const SEGMENT_SAMPLE_LIMIT: u64 = 24 * 3_600 * OUTPUT_RATE_HZ as u64;

/// The audio process's lock file in a call folder. A segment holds it for its whole life, and
/// repair takes it before it reads anything.
pub const LOCK_FILE_NAME: &str = "audio.lock";

/// How long a segment start waits for the folder's lock. A helper whose app died holds it until
/// its segment closes: 102 ms after its stdin closed, as measured. A repair holds it for
/// milliseconds. The rest is headroom for a slow disk.
const LOCK_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum StemWriterError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} reached the WAV size limit", path.display())]
    WavFull { path: PathBuf },
    #[error("could not read or write {}: {source}", path.display())]
    Manifest {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{} has schema_version {found}; this build reads up to {supported}", path.display())]
    ManifestVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error(
        "segment {number} cannot start in {}: segment {highest} already exists there",
        folder.display()
    )]
    SegmentNumberTaken {
        folder: PathBuf,
        number: u32,
        highest: u32,
    },
    #[error("{} is in use by another process", folder.display())]
    FolderBusy { folder: PathBuf },
    #[error("{} does not start with this writer's WAV header; repair left it as it is", path.display())]
    NotOwnWav { path: PathBuf },
    #[error("the {stem} stem reports an input rate of 0 Hz")]
    ZeroRate { stem: Stem },
    #[error("could not set up a resampler from {rate_hz} Hz: {source}")]
    ResamplerSetup {
        rate_hz: u32,
        #[source]
        source: rubato::ResamplerConstructionError,
    },
    #[error("the resampler failed: {source}")]
    Resample {
        #[source]
        source: rubato::ResampleError,
    },
    #[error("a resampler buffer has the wrong size: {source}")]
    BufferShape {
        #[source]
        source: rubato::audioadapter_buffers::SizeError,
    },
    #[error("the {stem} stem is already in segment {number}")]
    StemAlreadyAdded { stem: Stem, number: u32 },
    #[error("the {stem} stem was never added to segment {number}")]
    StemMissing { stem: Stem, number: u32 },
    #[error("the clock reading is outside the supported time range")]
    ClockOutOfRange,
}

/// Reads the host clock against UTC. The segment reads it at start, at each rotation and at
/// close.
pub type ClockReader = Box<dyn FnMut() -> UtcAnchor + Send>;

/// Something the helper reports to the app.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SegmentEvent {
    Gap {
        stem: Stem,
        #[serde(flatten)]
        gap: GapRecord,
    },
    /// A write to the stem's file failed, or the file reached the WAV size limit. The stem
    /// stopped; the other one keeps recording.
    StemStopped { stem: Stem, reason: String },
    /// A sync or a manifest store failed. The next checkpoint retries it.
    CheckpointFailed { reason: String },
    /// A stem reached `SEGMENT_SAMPLE_LIMIT`, so both stems moved to the next segment.
    Rotated { from_number: u32, to_number: u32 },
}

/// One numbered segment of a call folder, open for writing. Lives on an ordinary thread: never
/// call it from an audio callback.
pub struct Segment {
    folder: PathBuf,
    number: u32,
    read_clock: ClockReader,
    clock: UtcAnchor,
    clock_utc: Timestamp,
    manifest: AudioManifest,
    mic: Option<StemTrack>,
    remote: Option<StemTrack>,
    /// Stems that stopped in an earlier segment of this run. They take no more writes.
    retired: Vec<Stem>,
    events: Vec<SegmentEvent>,
    last_checkpoint: Instant,
    /// Held for the segment's life, so repair never touches the files it writes. Last, so it
    /// drops after the stem files flush their buffers.
    _lock: FolderLock,
}

/// What a finished segment holds.
#[derive(Debug, Clone)]
pub struct SegmentReport {
    pub number: u32,
    pub stems: Vec<StemProgress>,
    /// Events not yet taken when the segment finished.
    pub events: Vec<SegmentEvent>,
}

impl Segment {
    /// Starts segment `number` of the call folder. The number must be above every segment in
    /// `audio.json` and every `NNN-*.wav` in the folder: a finished segment is never reopened.
    /// `read_clock` gives each segment its clock readings. Takes the folder's lock first, then
    /// repairs any segment an earlier writer left open.
    ///
    /// # Errors
    ///
    /// Fails when another process holds the folder for longer than 3 s, the number is taken, the
    /// folder cannot be read or created, or `audio.json` is unreadable or from a newer schema.
    pub fn start(
        folder: &Path,
        number: u32,
        call_id: &str,
        platform: Platform,
        mut read_clock: ClockReader,
    ) -> Result<Self, StemWriterError> {
        std::fs::create_dir_all(folder).map_err(|source| StemWriterError::Io {
            path: folder.to_owned(),
            source,
        })?;
        let lock = FolderLock::take_within(folder, LOCK_FILE_NAME, LOCK_WAIT)
            .map_err(|source| StemWriterError::Io {
                path: folder.join(LOCK_FILE_NAME),
                source,
            })?
            .ok_or_else(|| StemWriterError::FolderBusy {
                folder: folder.to_owned(),
            })?;
        // A failed repair leaves the old segment as it was. The number check below still keeps
        // its files from being written again, so the call records on.
        if let Err(error) = repair::repair_locked(folder) {
            tracing::warn!(
                file.directory = %folder.display(),
                exception.message = %error,
                "repair.failed"
            );
        }
        let mut manifest = AudioManifest::load_or_new(folder)?;
        let highest = highest_segment_number(folder, &manifest)?;
        if number <= highest {
            return Err(StemWriterError::SegmentNumberTaken {
                folder: folder.to_owned(),
                number,
                highest,
            });
        }
        let clock = read_clock();
        let clock_utc = utc_timestamp(clock.utc)?;
        manifest.segments.push(SegmentRecord {
            number,
            call_id: Some(call_id.to_owned()),
            platform,
            state: SegmentState::Open,
            clock: Some(clock_record(clock, clock_utc)),
            closing_clock: None,
            closed_at: None,
            stems: Vec::new(),
            repair: None,
        });
        manifest.store(folder)?;
        tracing::info!(
            file.directory = %folder.display(),
            segment.number = number,
            call.id = call_id,
            "segment.started"
        );
        Ok(Self {
            folder: folder.to_owned(),
            number,
            read_clock,
            clock,
            clock_utc,
            manifest,
            mic: None,
            remote: None,
            retired: Vec::new(),
            events: Vec::new(),
            last_checkpoint: Instant::now(),
            _lock: lock,
        })
    }

    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }

    /// Whether the stem was added in this run, including one that stopped in an earlier segment.
    #[must_use]
    pub fn has_stem(&self, stem: Stem) -> bool {
        let slot = match stem {
            Stem::Mic => &self.mic,
            Stem::Remote => &self.remote,
        };
        slot.is_some() || self.retired.contains(&stem)
    }

    /// Creates the stem's file. `opened_rate_hz` is the rate the device opened at; the stamps'
    /// rates take over from the first buffer.
    ///
    /// # Errors
    ///
    /// Fails when the stem is already in this segment, its file exists, or the rate is 0.
    pub fn add_stem(
        &mut self,
        stem: Stem,
        device: &str,
        opened_rate_hz: u32,
    ) -> Result<(), StemWriterError> {
        if self.has_stem(stem) {
            return Err(StemWriterError::StemAlreadyAdded {
                stem,
                number: self.number,
            });
        }
        let file = stem_file_name(self.number, stem);
        let path = self.folder.join(&file);
        let track = StemTrack::create(stem, file, &path, device.to_owned(), opened_rate_hz)?;
        tracing::info!(
            capture.stem = %stem,
            device.name = device,
            audio.sample_rate_hz = opened_rate_hz,
            segment.number = self.number,
            "segment.stem_added"
        );
        *self.track_slot(stem) = Some(track);
        self.store_manifest();
        Ok(())
    }

    /// Writes one drain of a stem. Each stamp must refer to samples given in this call or an
    /// earlier one. A failed write stops this stem alone and becomes a `StemStopped` event. A
    /// stem at `SEGMENT_SAMPLE_LIMIT` moves both stems to the next segment.
    ///
    /// # Errors
    ///
    /// Fails only when the stem was never added.
    pub fn write(
        &mut self,
        stem: Stem,
        stamps: &[BufferStamp],
        samples: &[f32],
    ) -> Result<(), StemWriterError> {
        let Some(track) = self.writable_track(stem)? else {
            return Ok(());
        };
        let outcome = track.write(stamps, samples);
        self.collect_gaps(stem);
        if let Err(error) = outcome {
            self.stop_stem(stem, &error);
        }
        // Silence longer than a segment fills one file per rotation. The audio held behind it
        // goes into the new file.
        while self.writing_track(stem).is_some_and(StemTrack::is_full) {
            self.rotate();
            let outcome = self
                .track_slot(stem)
                .as_mut()
                .filter(|track| !track.is_stopped())
                .map(|track| track.write(&[], &[]));
            self.collect_gaps(stem);
            if let Some(Err(error)) = outcome {
                self.stop_stem(stem, &error);
            }
        }
        Ok(())
    }

    /// Moves the stem to a new device whose stream starts again at sample 0, so the new indexes
    /// do not read as drift. The time between the devices becomes a `device_switch` gap.
    ///
    /// # Errors
    ///
    /// Fails only when the stem was never added.
    pub fn switch_device(
        &mut self,
        stem: Stem,
        device: &str,
        rate_hz: u32,
    ) -> Result<(), StemWriterError> {
        let Some(track) = self.writable_track(stem)? else {
            return Ok(());
        };
        let outcome = track.switch_device(device.to_owned(), rate_hz);
        self.collect_gaps(stem);
        if let Err(error) = outcome {
            self.stop_stem(stem, &error);
        }
        Ok(())
    }

    /// Records the capture ring's loss counters, for the manifest.
    pub fn note_ring_losses(&mut self, stem: Stem, losses: StemLosses) {
        if let Some(track) = self.track_slot(stem).as_mut() {
            track.note_ring_losses(losses);
        }
    }

    /// Events since the last call: gaps, stopped stems, failed checkpoints, rotations.
    pub fn take_events(&mut self) -> Vec<SegmentEvent> {
        std::mem::take(&mut self.events)
    }

    /// Runs `checkpoint` once `CHECKPOINT_INTERVAL` has passed since the last one.
    pub fn checkpoint_if_due(&mut self) {
        if self.last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            self.checkpoint();
        }
    }

    /// Makes both stems durable, samples then header, then replaces `audio.json`. A failure is
    /// logged and becomes a `CheckpointFailed` event; the next checkpoint tries again. A file at
    /// the WAV size limit cannot be retried, so its stem stops.
    pub fn checkpoint(&mut self) {
        self.last_checkpoint = Instant::now();
        for stem in [Stem::Mic, Stem::Remote] {
            let outcome = self
                .track_slot(stem)
                .as_mut()
                .filter(|track| !track.is_stopped())
                .map(StemTrack::sync);
            match outcome {
                Some(Err(error @ StemWriterError::WavFull { .. })) => self.stop_stem(stem, &error),
                Some(Err(error)) => self.checkpoint_failed(&error),
                Some(Ok(())) | None => {}
            }
        }
        self.store_manifest();
    }

    /// The stem file's level since the last take. `None` while the stem has no file.
    pub fn take_level(&mut self, stem: Stem) -> Option<OutputLevel> {
        self.track_slot(stem).as_mut().map(StemTrack::take_level)
    }

    /// The stem's progress. Resets its recent peak.
    pub fn take_progress(&mut self, stem: Stem) -> Option<StemProgress> {
        let track = self.track_slot(stem).as_mut()?;
        let progress = track.progress();
        track.reset_recent_peak();
        Some(progress)
    }

    /// Writes all held audio, makes both files durable and marks the segment closed, with a
    /// second clock reading.
    ///
    /// # Errors
    ///
    /// Fails when `audio.json` cannot be stored. The files are complete by then; the segment
    /// stays `open` in the manifest, for repair at the next launch.
    pub fn finish(mut self) -> Result<SegmentReport, StemWriterError> {
        for stem in [Stem::Mic, Stem::Remote] {
            let outcome = self.track_slot(stem).as_mut().map(StemTrack::finish);
            self.collect_gaps(stem);
            if let Some(Err(error)) = outcome {
                self.stop_stem(stem, &error);
            }
        }
        let stems = [self.mic.as_ref(), self.remote.as_ref()]
            .into_iter()
            .flatten()
            .map(StemTrack::progress)
            .collect();
        let closing_clock = (self.read_clock)();
        let closing_utc = utc_timestamp(closing_clock.utc)?;
        let stem_records = self.stem_records();
        let closed_at = utc_timestamp(SystemTime::now())?;
        if let Some(segment) = self.segment_record_mut() {
            segment.stems = stem_records;
            segment.state = SegmentState::Closed;
            segment.closing_clock = Some(clock_record(closing_clock, closing_utc));
            segment.closed_at = Some(closed_at);
        }
        self.manifest.store(&self.folder)?;
        tracing::info!(segment.number = self.number, "segment.closed");
        Ok(SegmentReport {
            number: self.number,
            stems,
            events: std::mem::take(&mut self.events),
        })
    }

    /// Ends both stems' files and continues them in the next segment. A stopped stem, or one
    /// whose next file cannot be created, stays listed in the closed segment and leaves the new
    /// one. One clock reading closes the old segment and opens the new one.
    fn rotate(&mut self) {
        let from_number = self.number;
        let to_number = from_number + 1;
        for stem in [Stem::Mic, Stem::Remote] {
            let outcome = self
                .track_slot(stem)
                .as_mut()
                .filter(|track| !track.is_stopped())
                .map(StemTrack::end_file);
            self.collect_gaps(stem);
            if let Some(Err(error)) = outcome {
                self.stop_stem(stem, &error);
            }
        }
        let clock = (self.read_clock)();
        let clock_utc = utc_timestamp(clock.utc).unwrap_or(self.clock_utc);
        let ended = self.stem_records();
        let closed_at = utc_timestamp(SystemTime::now()).ok();
        let mut next = None;
        if let Some(segment) = self.segment_record_mut() {
            segment.stems = ended;
            segment.state = SegmentState::Closed;
            segment.closing_clock = Some(clock_record(clock, clock_utc));
            segment.closed_at = closed_at;
            next = Some(SegmentRecord {
                number: to_number,
                call_id: segment.call_id.clone(),
                platform: segment.platform,
                state: SegmentState::Open,
                clock: Some(clock_record(clock, clock_utc)),
                closing_clock: None,
                closed_at: None,
                stems: Vec::new(),
                repair: None,
            });
        }
        self.manifest.segments.extend(next);
        self.number = to_number;
        self.clock = clock;
        self.clock_utc = clock_utc;
        for stem in [Stem::Mic, Stem::Remote] {
            let file = stem_file_name(to_number, stem);
            let path = self.folder.join(&file);
            let outcome = match self.track_slot(stem).as_mut() {
                None => continue,
                Some(track) if track.is_stopped() => None,
                Some(track) => Some(track.start_file(file, &path)),
            };
            if let Some(Err(error)) = &outcome {
                self.stop_stem(stem, error);
            }
            if !matches!(outcome, Some(Ok(()))) {
                *self.track_slot(stem) = None;
                self.retired.push(stem);
            }
        }
        tracing::info!(
            segment.number = to_number,
            segment.previous_number = from_number,
            "segment.rotated"
        );
        self.events.push(SegmentEvent::Rotated {
            from_number,
            to_number,
        });
        self.store_manifest();
    }

    /// The stem's track when it takes writes; `None` when it stopped.
    fn writable_track(&mut self, stem: Stem) -> Result<Option<&mut StemTrack>, StemWriterError> {
        let number = self.number;
        let is_retired = self.retired.contains(&stem);
        match self.track_slot(stem).as_mut() {
            Some(track) if track.is_stopped() => Ok(None),
            Some(track) => Ok(Some(track)),
            None if is_retired => Ok(None),
            None => Err(StemWriterError::StemMissing { stem, number }),
        }
    }

    /// The stem's track while it still takes writes.
    fn writing_track(&self, stem: Stem) -> Option<&StemTrack> {
        let slot = match stem {
            Stem::Mic => self.mic.as_ref(),
            Stem::Remote => self.remote.as_ref(),
        };
        slot.filter(|track| !track.is_stopped())
    }

    fn collect_gaps(&mut self, stem: Stem) {
        let gaps = self
            .track_slot(stem)
            .as_mut()
            .map(StemTrack::take_new_gaps)
            .unwrap_or_default();
        self.events
            .extend(gaps.into_iter().map(|gap| SegmentEvent::Gap { stem, gap }));
    }

    fn stop_stem(&mut self, stem: Stem, error: &StemWriterError) {
        let reason = error.to_string();
        tracing::error!(
            capture.stem = %stem,
            exception.message = %reason,
            segment.number = self.number,
            "stem.stopped"
        );
        if let Some(track) = self.track_slot(stem).as_mut() {
            track.stop(reason.clone());
            // Best effort: keep what the file already holds playable.
            if let Err(sync_error) = track.sync() {
                tracing::warn!(capture.stem = %stem, exception.message = %sync_error, "stem.sync_failed");
            }
        }
        self.events.push(SegmentEvent::StemStopped { stem, reason });
    }

    fn checkpoint_failed(&mut self, error: &StemWriterError) {
        let reason = error.to_string();
        tracing::warn!(exception.message = %reason, segment.number = self.number, "segment.checkpoint_failed");
        self.events.push(SegmentEvent::CheckpointFailed { reason });
    }

    /// Stores the manifest. A failure is logged and becomes an event; the next checkpoint
    /// stores it again.
    fn store_manifest(&mut self) {
        let stems = self.stem_records();
        if let Some(segment) = self.segment_record_mut() {
            segment.stems = stems;
        }
        if let Err(error) = self.manifest.store(&self.folder) {
            self.checkpoint_failed(&error);
        }
    }

    fn stem_records(&self) -> Vec<StemRecord> {
        [self.mic.as_ref(), self.remote.as_ref()]
            .into_iter()
            .flatten()
            .map(|track| {
                let started_at = track.first_host_time_ns().and_then(|ns| self.utc_at(ns));
                track.record(started_at)
            })
            .collect()
    }

    /// UTC of a host time, through the segment's clock reading.
    fn utc_at(&self, host_time_ns: u64) -> Option<Timestamp> {
        let offset_ns = i128::from(host_time_ns) - i128::from(self.clock.host_time_ns);
        let offset = SignedDuration::from_nanos(i64::try_from(offset_ns).ok()?);
        self.clock_utc.checked_add(offset).ok()
    }

    fn segment_record_mut(&mut self) -> Option<&mut SegmentRecord> {
        let number = self.number;
        self.manifest
            .segments
            .iter_mut()
            .rev()
            .find(|segment| segment.number == number)
    }

    fn track_slot(&mut self, stem: Stem) -> &mut Option<StemTrack> {
        match stem {
            Stem::Mic => &mut self.mic,
            Stem::Remote => &mut self.remote,
        }
    }
}

impl SegmentRecord {
    /// Host time of a position in one of this segment's stem files.
    #[must_use]
    pub fn host_time_at(&self, stem: Stem, file_ms: f64) -> Option<u64> {
        self.stems
            .iter()
            .find(|record| record.stem == stem)?
            .host_time_at(file_ms)
    }
}

/// One past the highest segment number in the folder's `audio.json` or in any stem file name.
/// Video's files do not count: the app starts audio and video with one shared number, and takes
/// the call's next number from both processes' files.
///
/// # Errors
///
/// Fails when the folder or its manifest cannot be read.
pub fn next_segment_number(folder: &Path) -> Result<u32, StemWriterError> {
    if !folder.exists() {
        return Ok(1);
    }
    let manifest = AudioManifest::load_or_new(folder)?;
    Ok(highest_segment_number(folder, &manifest)? + 1)
}

/// The file name of one stem of segment `number`, such as `001-mic.wav`.
#[must_use]
pub fn stem_file_name(number: u32, stem: Stem) -> String {
    format!("{number:03}-{stem}.wav")
}

fn clock_record(clock: UtcAnchor, utc: Timestamp) -> ClockRecord {
    ClockRecord {
        host_time_ns: clock.host_time_ns,
        utc,
        read_gap_ns: clock.read_gap_ns,
    }
}

fn utc_timestamp(utc: SystemTime) -> Result<Timestamp, StemWriterError> {
    Timestamp::try_from(utc).map_err(|_| StemWriterError::ClockOutOfRange)
}

/// The highest segment number in the manifest or in any `NNN-*` name in the folder. A file whose
/// segment never reached the manifest still counts, so it is never overwritten.
fn highest_segment_number(folder: &Path, manifest: &AudioManifest) -> Result<u32, StemWriterError> {
    let io_error = |source| StemWriterError::Io {
        path: folder.to_owned(),
        source,
    };
    let mut highest = manifest
        .segments
        .iter()
        .map(|segment| segment.number)
        .max()
        .unwrap_or(0);
    for entry in std::fs::read_dir(folder).map_err(io_error)? {
        let name = entry.map_err(io_error)?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Only stem files count. The video process starts the same number at the same moment, so
        // its `NNN-video` folder must not make the number look taken.
        if let Some(number) = segment_number_of(name)
            && Stem::ALL
                .into_iter()
                .any(|stem| stem_file_name(number, stem) == name)
        {
            highest = highest.max(number);
        }
    }
    Ok(highest)
}

/// The segment number of a file named `NNN-...`: three or more digits, then a dash.
pub(crate) fn segment_number_of(file_name: &str) -> Option<u32> {
    let (prefix, _) = file_name.split_once('-')?;
    let is_number = prefix.len() >= 3 && prefix.bytes().all(|byte| byte.is_ascii_digit());
    is_number.then(|| prefix.parse().ok()).flatten()
}
