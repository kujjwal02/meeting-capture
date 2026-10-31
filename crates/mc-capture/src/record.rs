//! Records one segment of a call folder through the stem writer: 16 kHz stems placed by host
//! time, and `audio.json`. `record_segment` is the loop both `record` and `audio` run; each
//! passes a `SegmentObserver` that reports in its own format and says when to stop. The loop
//! also follows device changes: it swaps the mic and rebuilds the tap by the rules in
//! `devices`, inside the same segment, and closes the segment when the Mac goes to sleep.
//! A mic that delivers only exact zeros for 10 s gets a permission check; with the lid closed
//! it counts as lost, since macOS mutes the built-in mic then.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use capture::{
    BufferStamp, CallAudioSource, CaptureError, IdleSleepBlock, MicMoment, MicRequest,
    MicTransport, PermissionService, PermissionState, PowerEvent, SleepAck, Stem, StemLosses,
    StemReader,
};
use capture_protocol::DeviceChangeCause;
use serde::Serialize;
use stem_writer::{Segment, SegmentEvent, SegmentReport, StemProgress};

use crate::devices::{
    CandidateState, DeliveredRate, DeviceWatch, MicCandidate, MicSwap, RebuildDecision, Takeover,
};
use crate::{LineKind, MicChoice};

/// Drain often enough that a 65,536-sample ring at 48 kHz (1.4 s) never fills. Also bounds how
/// long a stop waits before the segment closes.
const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

/// What `pmset -g assertions` names while stems are open.
const SLEEP_BLOCK_REASON: &str = "Meeting Capture is recording a call";
/// A mic that delivered only digital zeros this long may have lost its permission: Core Audio
/// then delivers zeros with no error. A closed lid mutes the built-in mic the same way.
const MIC_ZERO_LIMIT: Duration = Duration::from_secs(10);
/// A tap that delivered only digital zeros this long while the mic heard a voice may have lost
/// its permission. A quiet call is silent too, hence the longer wait and the voice condition.
const REMOTE_ZERO_LIMIT: Duration = Duration::from_secs(60);
/// A mic peak at or above this counts as a voice for the tap's zero check.
const VOICE_PEAK: f32 = 0.05;

/// The CLI's `record`: one segment for a fixed length, with status lines.
pub(crate) struct RecordRequest {
    pub folder: PathBuf,
    pub segment: Option<u32>,
    pub call_id: Option<String>,
    pub platform: sources::Platform,
    pub length: Duration,
    pub mic: MicChoice,
}

/// One segment to record.
pub(crate) struct SegmentRequest<'a> {
    pub folder: &'a Path,
    pub number: u32,
    pub call_id: &'a str,
    pub platform: sources::Platform,
    pub mic: MicChoice,
    /// `CallStart` for the call's first segment only.
    pub mic_moment: MicMoment,
    /// The built-in mic the call's last segment skipped because the lid was closed.
    pub lid_muted_uid: Option<String>,
}

/// Why a segment ended early. Each step names its own failure, so the helper can report a
/// closed kind.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SegmentError {
    #[error("could not start the segment: {0:#}")]
    Start(anyhow::Error),
    #[error("could not open the devices: {0:#}")]
    Open(anyhow::Error),
    #[error("could not write the segment: {0:#}")]
    Write(anyhow::Error),
    #[error("could not close the segment: {0:#}")]
    Close(anyhow::Error),
    /// The observer could not report, so no one hears this process any more.
    #[error("could not report: {0:#}")]
    Report(anyhow::Error),
    /// The tap needed more rebuilds than one episode allows.
    #[error(
        "the system audio tap needed a rebuild after {rebuilds} rebuilds in one episode ({cause})"
    )]
    RebuildLimit {
        rebuilds: u32,
        cause: DeviceChangeCause,
    },
}

/// What the observer wants after a drain step.
pub(crate) enum LoopControl {
    Continue,
    Stop,
    /// Close the segment, then let the Mac sleep.
    Sleep(SleepAck),
    /// The Mac woke while the segment was open: a sleep this process did not hold. Close the
    /// segment, since host time hid the sleep and the devices may have reset.
    Woke,
}

/// Why a segment closed without an error.
pub(crate) enum SegmentEnd {
    Stopped,
    /// For a sleep. Allow it once the close is reported.
    Sleep(SleepAck),
    Woke,
}

/// A stem moved to another device inside the segment.
pub(crate) struct DeviceChange {
    pub stem: Stem,
    /// `None` when the stem had no device before.
    pub old_device: Option<String>,
    pub new_device: String,
    pub rate_hz: u32,
    pub cause: DeviceChangeCause,
}

/// A stem's first buffer in the segment.
pub(crate) struct StemStart {
    pub stem: Stem,
    pub device: String,
    pub rate_hz: u32,
    pub first_host_time_ns: u64,
}

/// What a recording loop tells the outside, and when it stops.
pub(crate) trait SegmentObserver {
    fn stem_started(&mut self, segment: u32, start: &StemStart) -> anyhow::Result<()>;
    fn segment_events(&mut self, segment: u32, events: Vec<SegmentEvent>) -> anyhow::Result<()>;
    fn device_changed(&mut self, segment: u32, change: &DeviceChange) -> anyhow::Result<()>;
    /// The mic is lost, or the segment started, with no safe input left. The mic stem records a
    /// gap until a later `device_changed`. `cause` is `None` at the segment's start.
    fn mic_unavailable(
        &mut self,
        segment: u32,
        old_device: Option<&str>,
        cause: Option<DeviceChangeCause>,
        message: &str,
    ) -> anyhow::Result<()>;
    fn permission_checked(
        &mut self,
        service: PermissionService,
        state: PermissionState,
    ) -> anyhow::Result<()>;
    /// Runs after each drain step and says whether to go on.
    fn after_drain(&mut self, live: &mut LiveSegment<'_>) -> anyhow::Result<LoopControl>;
    /// Idle sleep could not be held off. The segment records anyway.
    fn sleep_block_failed(&mut self, error: &CaptureError) -> anyhow::Result<()>;
}

/// The open segment, as an observer sees it between drains.
pub(crate) struct LiveSegment<'a> {
    segment: &'a mut Segment,
    mic: &'a RecordStem,
    remote: Option<&'a RecordStem>,
    started: Instant,
}

/// Checks both permissions and reports them.
fn check_permissions(observer: &mut impl SegmentObserver) -> Result<(), SegmentError> {
    for service in PermissionService::AUDIO {
        observer
            .permission_checked(service, capture::permission_state(service))
            .map_err(SegmentError::Report)?;
    }
    Ok(())
}

impl LiveSegment<'_> {
    pub fn number(&self) -> u32 {
        self.segment.number()
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The stem's status line. Resets its recent peak.
    fn status_line(&mut self, stem: Stem) -> Option<StemLine> {
        let record_stem = match stem {
            Stem::Mic => Some(self.mic),
            Stem::Remote => self.remote,
        }?;
        self.segment
            .take_progress(stem)
            .map(|progress| record_stem.status_line(progress))
    }

    /// The stem's counters. Resets its recent peak.
    pub fn counters(&mut self, stem: Stem) -> Option<capture_protocol::StreamCounters> {
        self.segment
            .take_progress(stem)
            .map(|progress| capture_protocol::StreamCounters {
                samples_in: progress.samples_in,
                samples_written: progress.samples_written,
            })
    }
}

/// A closed segment, with what the summary needs.
pub(crate) struct FinishedSegment {
    pub report: SegmentReport,
    pub end: SegmentEnd,
    pub stop_host_ns: u64,
    pub elapsed: Duration,
    /// The built-in mic skipped for a closed lid, for the call's next segment.
    pub lid_muted_uid: Option<String>,
    mic: RecordStem,
    remote: Option<RecordStem>,
}

#[derive(Serialize)]
struct StemLine {
    file: String,
    rate_in_hz: u32,
    samples_in: u64,
    samples_written: u64,
    duration_s: f64,
    gap_count: usize,
    gap_largest_ms: f64,
    gap_total_ms: f64,
    drift_ms: Option<f64>,
    steering_ppm: f64,
    ahead_count: usize,
    peak: f32,
    stopped_by: Option<String>,
    #[serde(flatten)]
    ring_losses: StemLosses,
}

#[derive(Serialize)]
struct StatusLine {
    kind: LineKind,
    elapsed_s: f64,
    segment: u32,
    mic: Option<StemLine>,
    remote: Option<StemLine>,
}

#[derive(Serialize)]
struct StemSummary {
    /// Host time from the file's first sample to the stop, in seconds.
    wall_s: Option<f64>,
    /// File length minus `wall_s`.
    duration_minus_wall_ms: Option<f64>,
    /// Samples the ring dropped before the writer's first drain.
    dropped_samples_at_start: u64,
    #[serde(flatten)]
    totals: StemLine,
}

#[derive(Serialize)]
struct SummaryLine {
    kind: LineKind,
    elapsed_s: f64,
    folder: String,
    segment: u32,
    mic: Option<StemSummary>,
    remote: Option<StemSummary>,
}

/// One stem's reader-side state: the reused drain buffers and the ring's loss counters.
struct RecordStem {
    stem: Stem,
    stamps: Vec<BufferStamp>,
    samples: Vec<f32>,
    /// The current reader's losses.
    losses: StemLosses,
    /// Losses of the readers this stem used before the current one.
    earlier_losses: StemLosses,
    dropped_samples_at_start: Option<u64>,
    /// Samples at the start of the current reader's stream that a mic takeover dropped. The
    /// reader's stamps count them; the writer must not.
    index_offset: u64,
    /// Host time where the newest buffer ended.
    last_end_ns: Option<u64>,
    first_delivery: Option<Instant>,
    last_delivery: Option<Instant>,
    /// The current reader's first delivery.
    reader_first_delivery: Option<Instant>,
    /// When a drain last held a sample that is not exactly zero.
    last_non_zero_at: Option<Instant>,
    /// The current run of zeros already had its permission check.
    is_zero_run_checked: bool,
    peak: f32,
    /// The peak of the newest drain alone.
    drain_peak: f32,
    /// Set when the stem's file could not be added. The ring is still drained and discarded.
    is_stopped: bool,
    is_start_reported: bool,
    /// The first buffer, until the loop hands it to the observer.
    pending_start: Option<StemStart>,
    /// Events from adding the stem, until the loop hands them to the observer.
    pending_events: Vec<SegmentEvent>,
}

impl RecordStem {
    fn new(stem: Stem) -> Self {
        Self {
            stem,
            stamps: Vec::new(),
            samples: Vec::new(),
            losses: StemLosses::default(),
            earlier_losses: StemLosses::default(),
            dropped_samples_at_start: None,
            index_offset: 0,
            last_end_ns: None,
            first_delivery: None,
            last_delivery: None,
            reader_first_delivery: None,
            last_non_zero_at: None,
            is_zero_run_checked: false,
            peak: 0.0,
            drain_peak: 0.0,
            is_stopped: false,
            is_start_reported: false,
            pending_start: None,
            pending_events: Vec::new(),
        }
    }

    /// Moves everything queued in the ring into the segment. Adds the stem's file on the first
    /// drain, when the device's name and rate are known. A stem whose file cannot be added stops
    /// alone, with a `stem_stopped` event; the other stem keeps recording.
    fn drain(&mut self, segment: &mut Segment, reader: &mut StemReader) -> anyhow::Result<()> {
        if !self.is_stopped && !segment.has_stem(self.stem) {
            let format = reader.format();
            if let Err(error) =
                segment.add_stem(self.stem, &format.device_name, format.sample_rate_hz)
            {
                self.is_stopped = true;
                tracing::error!(
                    capture.stem = %self.stem,
                    exception.message = %error,
                    "stem.add_failed"
                );
                self.pending_events.push(SegmentEvent::StemStopped {
                    stem: self.stem,
                    reason: error.to_string(),
                });
            }
        }
        self.stamps.clear();
        self.samples.clear();
        let stamps = &mut self.stamps;
        let samples = &mut self.samples;
        let index_offset = self.index_offset;
        reader.drain(
            |stamp| {
                if let Some(sample_index) = stamp.sample_index.checked_sub(index_offset) {
                    stamps.push(BufferStamp {
                        sample_index,
                        ..stamp
                    });
                }
            },
            |chunk| samples.extend_from_slice(chunk),
        );
        self.losses = reader.losses();
        self.dropped_samples_at_start
            .get_or_insert(self.losses.dropped_samples);
        self.write_drained(segment, &reader.format().device_name)
    }

    /// Writes what `stamps` and `samples` hold, and notes when it arrived.
    fn write_drained(&mut self, segment: &mut Segment, device: &str) -> anyhow::Result<()> {
        let now = Instant::now();
        if let Some(last) = self.stamps.last() {
            self.last_end_ns = Some(crate::devices::buffer_end_ns(last));
        }
        // A buffer with no valid host time has samples but no stamp; it still shows the device
        // delivers.
        if !self.stamps.is_empty() || !self.samples.is_empty() {
            self.last_delivery = Some(now);
            self.first_delivery.get_or_insert(now);
            self.reader_first_delivery.get_or_insert(now);
        }
        #[cfg(debug_assertions)]
        if self.stem == Stem::Mic && crate::inject::is_mic_zeroed() {
            self.samples.fill(0.0);
        }
        self.drain_peak = self
            .samples
            .iter()
            .fold(0.0, |peak, sample| peak.max(sample.abs()));
        self.peak = self.peak.max(self.drain_peak);
        if self.drain_peak > 0.0 {
            self.last_non_zero_at = Some(now);
            self.is_zero_run_checked = false;
        }
        if self.is_stopped {
            return Ok(());
        }
        if !self.is_start_reported
            && let Some(first) = self.stamps.first()
        {
            self.is_start_reported = true;
            self.pending_start = Some(StemStart {
                stem: self.stem,
                device: device.to_owned(),
                rate_hz: first.sample_rate_hz,
                first_host_time_ns: first.host_time_ns,
            });
        }
        segment.note_ring_losses(self.stem, self.total_losses());
        segment.write(self.stem, &self.stamps, &self.samples)?;
        Ok(())
    }

    /// Moves to a new reader: its losses start at 0 and its stamps count from its own start.
    fn start_reader(&mut self, index_offset: u64) {
        self.earlier_losses = self.earlier_losses + self.losses;
        self.losses = StemLosses::default();
        self.index_offset = index_offset;
        self.reader_first_delivery = None;
        self.is_zero_run_checked = false;
    }

    /// How long the current reader has delivered only exact zeros, while it still delivers.
    fn zero_run(&self, now: Instant) -> Option<Duration> {
        let is_delivering = self
            .last_delivery
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1));
        let reader_start = self.reader_first_delivery.filter(|_| is_delivering)?;
        let run_start = self
            .last_non_zero_at
            .map_or(reader_start, |at| at.max(reader_start));
        Some(now.duration_since(run_start))
    }

    fn total_losses(&self) -> StemLosses {
        self.earlier_losses + self.losses
    }

    /// Hands the first buffer and any add failure to the observer.
    fn report(
        &mut self,
        number: u32,
        observer: &mut impl SegmentObserver,
    ) -> Result<(), SegmentError> {
        if !self.pending_events.is_empty() {
            observer
                .segment_events(number, std::mem::take(&mut self.pending_events))
                .map_err(SegmentError::Report)?;
        }
        if let Some(start) = self.pending_start.take() {
            observer
                .stem_started(number, &start)
                .map_err(SegmentError::Report)?;
        }
        Ok(())
    }

    fn line(progress: StemProgress, peak: f32, ring_losses: StemLosses) -> StemLine {
        StemLine {
            file: progress.file,
            rate_in_hz: progress.rate_in_hz,
            samples_in: progress.samples_in,
            samples_written: progress.samples_written,
            duration_s: seconds_of_output(progress.samples_written),
            gap_count: progress.gap_count,
            gap_largest_ms: progress.gap_largest_ms,
            gap_total_ms: progress.gap_total_ms,
            drift_ms: progress.drift_ms,
            steering_ppm: progress.steering_ppm,
            ahead_count: progress.ahead_count,
            peak,
            stopped_by: progress.stopped_by,
            ring_losses,
        }
    }

    fn status_line(&self, progress: StemProgress) -> StemLine {
        let peak = progress.peak_recent;
        Self::line(progress, peak, self.total_losses())
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "host-time spans within a call stay far below 2^52 ns"
    )]
    fn summary(&self, progress: StemProgress, stop_host_ns: u64) -> StemSummary {
        let wall_s = progress
            .first_host_time_ns
            .map(|first_ns| stop_host_ns.saturating_sub(first_ns) as f64 / 1e9);
        let peak = progress.peak;
        let totals = Self::line(progress, peak, self.total_losses());
        StemSummary {
            wall_s,
            duration_minus_wall_ms: wall_s.map(|wall_s| (totals.duration_s - wall_s) * 1e3),
            dropped_samples_at_start: self.dropped_samples_at_start.unwrap_or(0),
            totals,
        }
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "sample counts stay far below 2^52"
)]
fn seconds_of_output(samples: u64) -> f64 {
    samples as f64 / f64::from(stem_writer::OUTPUT_RATE_HZ)
}

fn print_line(line: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(line)?);
    Ok(())
}

/// A stem's reader, or none while its device is closed: after a failed reopen, or while no
/// safe mic exists.
struct StemFeed {
    stem: RecordStem,
    reader: Option<StemReader>,
    /// When the current reader's device opened. A device that never delivers stalls from here.
    reader_opened_at: Instant,
    /// The name of the device the newest reader came from. `None` before any.
    device_name: Option<String>,
    /// Debug builds' injected event: the reader's buffers are thrown away.
    is_silenced: bool,
}

impl StemFeed {
    fn new(stem: RecordStem, reader: Option<StemReader>) -> Self {
        let is_silenced = stem.stem == Stem::Remote && is_tap_silenced();
        Self {
            stem,
            device_name: reader
                .as_ref()
                .map(|reader| reader.format().device_name.clone()),
            is_silenced,
            reader,
            reader_opened_at: Instant::now(),
        }
    }

    fn drain(&mut self, segment: &mut Segment) -> anyhow::Result<()> {
        if let Some(reader) = self.reader.as_mut() {
            if !self.is_silenced {
                return self.stem.drain(segment, reader);
            }
            reader.drain(|_| {}, |_| {});
        }
        // The rate check reads `stamps` after each drain; old ones would count twice.
        self.stem.stamps.clear();
        self.stem.samples.clear();
        self.stem.drain_peak = 0.0;
        Ok(())
    }
}

/// Both stems' feeds, and the tap's delivered-rate check.
struct Feeds {
    mic: StemFeed,
    remote: Option<StemFeed>,
    remote_rate: DeliveredRate,
    /// The mic's peak since the tap's current run of zeros began: a voice then means the tap
    /// should have heard something.
    mic_peak_in_remote_zeros: f32,
}

impl Feeds {
    fn drain(&mut self, segment: &mut Segment) -> anyhow::Result<()> {
        self.mic.drain(segment)?;
        if let Some(remote) = self.remote.as_mut() {
            remote.drain(segment)?;
            self.remote_rate.observe(&remote.stem.stamps);
        }
        Ok(())
    }

    fn report(
        &mut self,
        number: u32,
        observer: &mut impl SegmentObserver,
    ) -> Result<(), SegmentError> {
        self.mic.stem.report(number, observer)?;
        if let Some(remote) = self.remote.as_mut() {
            remote.stem.report(number, observer)?;
        }
        Ok(())
    }
}

/// The open devices and the rules that move them.
struct LiveDevices {
    source: CallAudioSource,
    feeds: Feeds,
    candidate: Option<MicCandidate>,
    watch: DeviceWatch,
    /// Why the mic counts as lost, when it was not its device that went: a closed lid.
    mic_lost_cause: Option<DeviceChangeCause>,
}

/// Records one segment until the observer says stop: opens the mic, then the tap, drains both
/// every 50 ms, follows device changes, then closes the segment in teardown order. Idle sleep
/// stays off while the stems are open.
pub(crate) fn record_segment(
    request: &SegmentRequest<'_>,
    observer: &mut impl SegmentObserver,
) -> Result<FinishedSegment, SegmentError> {
    let folder = request.folder;
    let mut segment = Segment::start(
        folder,
        request.number,
        request.call_id,
        request.platform,
        Box::new(capture::utc_anchor),
    )
    .map_err(|error| {
        SegmentError::Start(anyhow::Error::new(error).context(format!(
            "start segment {} in {}",
            request.number,
            folder.display()
        )))
    })?;
    let sleep_block = match IdleSleepBlock::hold(SLEEP_BLOCK_REASON) {
        Ok(block) => Some(block),
        Err(error) => {
            tracing::warn!(exception.message = %error, "power.assertion_failed");
            if let Err(error) = observer.sleep_block_failed(&error) {
                return Err(close_after_failed_open(
                    segment,
                    SegmentError::Report(error),
                    observer,
                ));
            }
            None
        }
    };
    if let Err(error) = check_permissions(observer) {
        return Err(close_after_failed_open(segment, error, observer));
    }
    // The opens below read the devices fresh, so changes from before this segment are spent.
    let _spent = capture::take_hal_changes();
    let (mut devices, mic_error) = match open_devices(&mut segment, request, observer) {
        Ok(opened) => opened,
        Err(error) => return Err(close_after_failed_open(segment, error, observer)),
    };

    let started = Instant::now();
    let recording = match mic_error {
        Some(error) => {
            tracing::warn!(exception.message = %error, "mic.unavailable_at_start");
            observer
                .mic_unavailable(request.number, None, None, &error.to_string())
                .map_err(SegmentError::Report)
        }
        None => Ok(()),
    }
    .and_then(|()| record_until_stopped(&mut segment, started, observer, &mut devices));

    // Teardown order: no callback runs after this, so the last drain holds every sample.
    devices.source.stop_callbacks();
    drop(devices.candidate.take());
    let stop_host_ns = capture::host_now_ns();
    let last_drain = devices.feeds.drain(&mut segment);
    let finished = segment.finish();
    drop(devices.source);
    drop(sleep_block);
    let report = finished.map_err(|error| SegmentError::Close(error.into()))?;
    let end = recording?;
    last_drain.map_err(SegmentError::Write)?;
    Ok(FinishedSegment {
        report,
        end,
        stop_host_ns,
        elapsed: started.elapsed(),
        lid_muted_uid: devices.watch.lid_muted_uid(),
        mic: devices.feeds.mic.stem,
        remote: devices.feeds.remote.map(|feed| feed.stem),
    })
}

/// Opens the mic, then the tap, while the segment drains the mic. The error beside the devices
/// says why the call has no safe mic, when it has none.
fn open_devices(
    segment: &mut Segment,
    request: &SegmentRequest<'_>,
    observer: &mut impl SegmentObserver,
) -> Result<(LiveDevices, Option<CaptureError>), SegmentError> {
    let mut watch = DeviceWatch::new(request.mic.rule(), request.lid_muted_uid.clone());
    let mic_request = watch.mic_request(request.mic_moment);
    let mut mic_stem = RecordStem::new(Stem::Mic);
    let mut drain_outcome = Ok(());
    let opened = CallAudioSource::open(mic_request, |mic_reader| {
        if drain_outcome.is_ok() {
            drain_outcome = mic_stem.drain(segment, mic_reader);
            segment.checkpoint_if_due();
        }
    });
    let (source, readers) = match (opened, drain_outcome) {
        (Ok(opened), Ok(())) => opened,
        (Err(error), _) => {
            // A refused permission is the likeliest cause a check can name. The open error is
            // the one to return, so a failed report here only goes to the log.
            if let Err(report_error) = check_permissions(observer) {
                tracing::error!(exception.message = %report_error, "permission.report_failed");
            }
            return Err(SegmentError::Open(error.into()));
        }
        (Ok(opened), Err(error)) => {
            drop(opened);
            return Err(SegmentError::Write(error));
        }
    };
    let (mic_reader, mic_error) = match readers.mic {
        Ok(opened) => (Some(opened.reader), None),
        Err(error) => (None, Some(error)),
    };
    let devices = LiveDevices {
        source,
        feeds: Feeds {
            mic: StemFeed::new(mic_stem, mic_reader),
            remote: readers
                .remote
                .map(|opened| StemFeed::new(RecordStem::new(Stem::Remote), Some(opened.reader))),
            remote_rate: DeliveredRate::new(),
            mic_peak_in_remote_zeros: 0.0,
        },
        candidate: None,
        watch,
        mic_lost_cause: None,
    };
    Ok((devices, mic_error))
}

/// Drains both stems every 50 ms and follows device changes until the observer says stop. The
/// caller finishes the segment whatever this returns.
fn record_until_stopped(
    segment: &mut Segment,
    started: Instant,
    observer: &mut impl SegmentObserver,
    devices: &mut LiveDevices,
) -> Result<SegmentEnd, SegmentError> {
    loop {
        devices.feeds.drain(segment).map_err(SegmentError::Write)?;
        if let Some(candidate) = devices.candidate.as_mut() {
            candidate.collect();
        }
        segment.checkpoint_if_due();
        let number = segment.number();
        devices.feeds.report(number, observer)?;
        observer
            .segment_events(number, segment.take_events())
            .map_err(SegmentError::Report)?;
        follow_devices(segment, observer, devices)?;
        check_digital_silence(devices, observer)?;
        let mut live = LiveSegment {
            segment: &mut *segment,
            mic: &devices.feeds.mic.stem,
            remote: devices.feeds.remote.as_ref().map(|feed| &feed.stem),
            started,
        };
        match observer
            .after_drain(&mut live)
            .map_err(SegmentError::Report)?
        {
            LoopControl::Continue => {}
            LoopControl::Stop => return Ok(SegmentEnd::Stopped),
            LoopControl::Sleep(ack) => return Ok(SegmentEnd::Sleep(ack)),
            LoopControl::Woke => return Ok(SegmentEnd::Woke),
        }
        std::thread::sleep(DRAIN_INTERVAL);
    }
}

/// Applies the device rules once: hands the mic to a ready new stream, opens one when the rule
/// asks, and rebuilds the tap on evidence.
fn follow_devices(
    segment: &mut Segment,
    observer: &mut impl SegmentObserver,
    devices: &mut LiveDevices,
) -> Result<(), SegmentError> {
    let LiveDevices {
        source,
        feeds,
        candidate,
        watch,
        mic_lost_cause,
    } = devices;
    #[cfg(debug_assertions)]
    apply_injected_events(source);
    let now = Instant::now();
    let changes = watch.take_changes(now);
    if changes.default_output || changes.device_list {
        feeds.remote_rate.log_next_window();
    }
    if changes.device_list
        && let Some(remote) = source.remote()
    {
        remote.check_alive();
    }

    let lost_cause = source
        .mic()
        .is_none_or(capture::MicStream::is_lost)
        .then(|| mic_lost_cause.unwrap_or(DeviceChangeCause::MicLost));
    if let Some(waiting) = candidate.as_ref() {
        match waiting.state(now) {
            CandidateState::Waiting => {}
            CandidateState::Ready => {
                if let Some(ready) = candidate.take() {
                    take_over_mic(segment, observer, source, feeds, ready)?;
                    *mic_lost_cause = None;
                }
            }
            CandidateState::NoBuffers => {
                let cause = waiting.cause;
                tracing::error!(device.change_cause = %cause, "mic.swap_no_buffers");
                drop(candidate.take());
                watch.mic_open_failed(now);
                // The old stream is gone too, so the stem has no mic until a later try.
                if cause != DeviceChangeCause::MicPickChanged {
                    let message = "the new mic stream delivered no buffer within 5 s";
                    drop_mic(segment, observer, source, feeds, cause, message)?;
                }
            }
        }
    } else if let Some(swap) = watch.mic_swap(now, source.mic(), lost_cause) {
        match swap {
            MicSwap::Open(cause, request) => {
                *candidate =
                    open_mic_candidate(segment, observer, source, feeds, watch, cause, request)?;
            }
            MicSwap::Unavailable(cause, error) => {
                drop_mic(segment, observer, source, feeds, cause, &error.to_string())?;
            }
        }
    }

    let health = source.remote().map(capture::RemoteSource::health);
    // A rebuilt tap's quiet time counts from its open, not from the old tap's last buffer.
    let last_delivery = feeds.remote.as_ref().map(|feed| {
        feed.stem
            .last_delivery
            .map_or(feed.reader_opened_at, |at| at.max(feed.reader_opened_at))
    });
    let is_rate_mismatched = feeds.remote_rate.is_mismatched();
    if let Some(cause) = watch.tap_rebuild_cause(now, health, last_delivery, is_rate_mismatched) {
        match watch.take_rebuild(now) {
            RebuildDecision::Allowed => {
                rebuild_tap(segment, observer, source, feeds, watch, cause)?;
            }
            RebuildDecision::CoolingDown => {}
            RebuildDecision::Exhausted => {
                return Err(SegmentError::RebuildLimit {
                    rebuilds: watch.rebuild_count(),
                    cause,
                });
            }
        }
    }
    Ok(())
}

/// Opens a second mic stream beside the recording one. The loop keeps draining both stems
/// while it opens. A failed open is logged and tried again later. The old stream records on,
/// unless it is lost: then the stem has no mic until a later try opens one.
fn open_mic_candidate(
    segment: &mut Segment,
    observer: &mut impl SegmentObserver,
    source: &mut CallAudioSource,
    feeds: &mut Feeds,
    watch: &mut DeviceWatch,
    cause: DeviceChangeCause,
    request: MicRequest,
) -> Result<Option<MicCandidate>, SegmentError> {
    tracing::info!(device.change_cause = %cause, "mic.swap_opening");
    let mut drained = Ok(());
    let opened = CallAudioSource::open_mic(request, || {
        if drained.is_ok() {
            drained = feeds.drain(segment);
            segment.checkpoint_if_due();
        }
    });
    drained.map_err(SegmentError::Write)?;
    match opened {
        Ok((stream, opened)) => {
            tracing::info!(
                device.name = stream.pick().device_name.as_deref(),
                device.change_cause = %cause,
                device.open_ms = u64::try_from(opened.open_duration.as_millis()).unwrap_or(0),
                "mic.swap_opened"
            );
            Ok(Some(MicCandidate::new(stream, opened.reader, cause)))
        }
        Err(error) => {
            tracing::error!(
                exception.message = %error,
                device.change_cause = %cause,
                "mic.swap_open_failed"
            );
            watch.mic_open_failed(Instant::now());
            check_permissions(observer)?;
            if cause != DeviceChangeCause::MicPickChanged {
                drop_mic(segment, observer, source, feeds, cause, &error.to_string())?;
            }
            Ok(None)
        }
    }
}

/// Stops and releases the lost mic and leaves the stem with no reader, so it records a gap.
/// Reports once: a stem with no reader is already unavailable.
fn drop_mic(
    segment: &mut Segment,
    observer: &mut impl SegmentObserver,
    source: &mut CallAudioSource,
    feeds: &mut Feeds,
    cause: DeviceChangeCause,
    message: &str,
) -> Result<(), SegmentError> {
    if feeds.mic.reader.is_none() {
        return Ok(());
    }
    source.stop_mic();
    feeds.mic.drain(segment).map_err(SegmentError::Write)?;
    source.close_mic();
    feeds.mic.reader = None;
    feeds.mic.stem.start_reader(0);
    tracing::warn!(
        device.previous_name = feeds.mic.device_name.as_deref(),
        device.change_cause = %cause,
        exception.message = message,
        "mic.unavailable"
    );
    observer
        .mic_unavailable(
            segment.number(),
            feeds.mic.device_name.as_deref(),
            Some(cause),
            message,
        )
        .map_err(SegmentError::Report)
}

/// Make-before-break: stops the old mic stream only now that the new one delivered, writes
/// the old stream's last buffers, then continues the stem from the new stream's first buffer
/// that holds sound and starts after the old one's end.
fn take_over_mic(
    segment: &mut Segment,
    observer: &mut impl SegmentObserver,
    source: &mut CallAudioSource,
    feeds: &mut Feeds,
    mut candidate: MicCandidate,
) -> Result<(), SegmentError> {
    let cause = candidate.cause;
    let old_device = feeds.mic.device_name.clone();
    source.stop_mic();
    feeds.mic.drain(segment).map_err(SegmentError::Write)?;
    // Buffers that arrived while the old stream stopped belong to the new one.
    candidate.collect();
    let has_sound = candidate.has_sound();
    let Takeover {
        stream,
        reader,
        stamps,
        samples,
        dropped_samples,
    } = candidate.take_over(feeds.mic.stem.last_end_ns);
    source.replace_mic(stream);
    let new_device = reader.format().device_name.clone();
    let rate_hz = stamps
        .first()
        .map_or(reader.format().sample_rate_hz, |stamp| stamp.sample_rate_hz);
    // A segment that started with no mic adds the stem here.
    let switched = if segment.has_stem(Stem::Mic) {
        segment.switch_device(Stem::Mic, &new_device, rate_hz)
    } else {
        segment.add_stem(Stem::Mic, &new_device, rate_hz)
    };
    switched.map_err(|error| SegmentError::Write(error.into()))?;
    let stem = &mut feeds.mic.stem;
    stem.start_reader(dropped_samples);
    stem.losses = reader.losses();
    stem.stamps = stamps;
    stem.samples = samples;
    stem.write_drained(segment, &new_device)
        .map_err(SegmentError::Write)?;
    feeds.mic.reader = Some(reader);
    feeds.mic.reader_opened_at = Instant::now();
    feeds.mic.device_name = Some(new_device.clone());
    tracing::info!(
        device.previous_name = old_device.as_deref(),
        device.name = new_device,
        device.change_cause = %cause,
        mic.has_sound = has_sound,
        mic.dropped_samples = dropped_samples,
        "mic.swapped"
    );
    observer
        .device_changed(
            segment.number(),
            &DeviceChange {
                stem: Stem::Mic,
                old_device,
                new_device,
                rate_hz,
                cause,
            },
        )
        .map_err(SegmentError::Report)
}

/// Closes the tap, writes its last buffers, and opens a new one. The mic keeps draining while
/// the new tap opens. A failed open leaves the tap closed until the next try.
fn rebuild_tap(
    segment: &mut Segment,
    observer: &mut impl SegmentObserver,
    source: &mut CallAudioSource,
    feeds: &mut Feeds,
    watch: &mut DeviceWatch,
    cause: DeviceChangeCause,
) -> Result<(), SegmentError> {
    let Some(remote) = feeds.remote.as_mut() else {
        return Ok(());
    };
    tracing::warn!(
        device.change_cause = %cause,
        tap.rebuilds_in_episode = watch.rebuild_count(),
        "tap.rebuilding"
    );
    source.close_remote();
    remote.drain(segment).map_err(SegmentError::Write)?;
    if remote.reader.take().is_some() {
        remote.stem.start_reader(0);
    }
    let mut drained = Ok(());
    let mic = &mut feeds.mic;
    let reopened = source.reopen_remote(|| {
        if drained.is_ok() {
            drained = mic.drain(segment);
            segment.checkpoint_if_due();
        }
    });
    drained.map_err(SegmentError::Write)?;
    let opened = match reopened {
        Ok(opened) => opened,
        Err(error) => {
            tracing::error!(
                exception.message = %error,
                device.change_cause = %cause,
                "tap.reopen_failed"
            );
            watch.tap_open_failed(cause);
            return check_permissions(observer);
        }
    };
    let old_device = remote.device_name.clone();
    let new_device = opened.reader.format().device_name.clone();
    let rate_hz = opened.reader.format().sample_rate_hz;
    if segment.has_stem(Stem::Remote) {
        segment
            .switch_device(Stem::Remote, &new_device, rate_hz)
            .map_err(|error| SegmentError::Write(error.into()))?;
    }
    remote.reader = Some(opened.reader);
    remote.reader_opened_at = Instant::now();
    remote.is_silenced = is_tap_silenced();
    remote.device_name = Some(new_device.clone());
    feeds.remote_rate = DeliveredRate::new();
    feeds.mic_peak_in_remote_zeros = 0.0;
    watch.tap_rebuilt();
    tracing::info!(
        device.name = new_device,
        device.change_cause = %cause,
        device.open_ms = u64::try_from(opened.open_duration.as_millis()).unwrap_or(0),
        audio.sample_rate_hz = rate_hz,
        "tap.rebuilt"
    );
    observer
        .device_changed(
            segment.number(),
            &DeviceChange {
                stem: Stem::Remote,
                old_device,
                new_device,
                rate_hz,
                cause,
            },
        )
        .map_err(SegmentError::Report)
}

#[cfg(debug_assertions)]
fn is_tap_silenced() -> bool {
    crate::inject::take_tap_silenced()
}

#[cfg(not(debug_assertions))]
fn is_tap_silenced() -> bool {
    false
}

/// Core Audio hands a process without the permission digital zeros and no error, so each run
/// of zeros gets one permission check: 10 s on the mic, 60 s on the tap while the mic heard a
/// voice. A closed lid mutes the built-in mic to zeros too, so with the lid closed the mic
/// counts as lost and the next pick leaves it out.
fn check_digital_silence(
    devices: &mut LiveDevices,
    observer: &mut impl SegmentObserver,
) -> Result<(), SegmentError> {
    let now = Instant::now();
    let mic = &mut devices.feeds.mic.stem;
    if !mic.is_zero_run_checked && mic.zero_run(now).is_some_and(|run| run >= MIC_ZERO_LIMIT) {
        mic.is_zero_run_checked = true;
        tracing::warn!(capture.stem = %Stem::Mic, "stem.digital_silence");
        let service = PermissionService::Microphone;
        let state = capture::permission_state(service);
        observer
            .permission_checked(service, state)
            .map_err(SegmentError::Report)?;
        if let Some(mic) = devices.source.mic()
            && state != PermissionState::Denied
            && mic.pick().transport == MicTransport::BuiltIn
            && crate::devices::is_lid_closed() == Some(true)
        {
            let pick = mic.pick();
            tracing::warn!(device.name = pick.device_name.as_deref(), "mic.lid_muted");
            devices.watch.skip_lid_muted_mic(pick.device_uid.clone());
            devices.mic_lost_cause = Some(DeviceChangeCause::MicLidClosed);
            mic.mark_lost();
        }
    }
    let feeds = &mut devices.feeds;
    let Some(remote) = feeds.remote.as_mut() else {
        return Ok(());
    };
    // Sound on the tap ends its run of zeros, and with it the voice the mic heard in the run.
    feeds.mic_peak_in_remote_zeros = if remote.stem.drain_peak > 0.0 {
        0.0
    } else {
        feeds
            .mic_peak_in_remote_zeros
            .max(feeds.mic.stem.drain_peak)
    };
    if !remote.stem.is_zero_run_checked
        && feeds.mic_peak_in_remote_zeros >= VOICE_PEAK
        && remote
            .stem
            .zero_run(now)
            .is_some_and(|run| run >= REMOTE_ZERO_LIMIT)
    {
        remote.stem.is_zero_run_checked = true;
        tracing::warn!(capture.stem = %Stem::Remote, "stem.digital_silence");
        let service = PermissionService::AudioCapture;
        observer
            .permission_checked(service, capture::permission_state(service))
            .map_err(SegmentError::Report)?;
    }
    Ok(())
}

/// Applies the injected device events that are due.
#[cfg(debug_assertions)]
fn apply_injected_events(source: &mut CallAudioSource) {
    use capture::HalChanges;

    use crate::inject::DeviceEvent;

    let output_changed = HalChanges {
        default_output: true,
        ..HalChanges::default()
    };
    for event in crate::inject::take_due_device_events() {
        tracing::info!(inject.event = %event, "inject.device");
        match event {
            DeviceEvent::DefaultInput => capture::inject_hal_changes(HalChanges {
                default_input: true,
                ..HalChanges::default()
            }),
            DeviceEvent::DefaultOutput => capture::inject_hal_changes(output_changed),
            DeviceEvent::DeviceList => capture::inject_hal_changes(HalChanges {
                device_list: true,
                ..HalChanges::default()
            }),
            DeviceEvent::MicLost => {
                if let Some(mic) = source.mic() {
                    mic.mark_lost();
                }
            }
            DeviceEvent::MicZeros => crate::inject::set_mic_zeroed(true),
            DeviceEvent::MicSound => crate::inject::set_mic_zeroed(false),
            DeviceEvent::LidClosed => crate::inject::set_lid_closed(true),
            DeviceEvent::LidOpen => crate::inject::set_lid_closed(false),
            DeviceEvent::CandidateNoBuffers => crate::inject::silence_next_candidate(),
            DeviceEvent::TapSilentOpen => crate::inject::silence_next_tap(),
            DeviceEvent::TapFormat => {
                if let Some(remote) = source.remote() {
                    remote.mark_format_changed();
                }
            }
            DeviceEvent::TapStall => {
                if let Some(remote) = source.remote_mut() {
                    remote.stall_for_test();
                }
                capture::inject_hal_changes(output_changed);
            }
            DeviceEvent::TapStallQuiet => {
                if let Some(remote) = source.remote_mut() {
                    remote.stall_for_test();
                }
            }
            DeviceEvent::TapDead => {
                if let Some(remote) = source.remote_mut() {
                    remote.destroy_aggregate_for_test();
                }
            }
            DeviceEvent::HalProbe => capture::probe_device_list_listener(),
            DeviceEvent::VideoStall | DeviceEvent::VideoStill | DeviceEvent::VideoResume => {
                tracing::warn!(inject.event = %event, "inject.not_for_audio");
            }
        }
    }
}

/// Closes a segment whose devices did not all open, with whatever the mic wrote, so no empty
/// open segment is left behind. Returns the open error; a failed close is logged beside it.
fn close_after_failed_open(
    segment: Segment,
    open_error: SegmentError,
    observer: &mut impl SegmentObserver,
) -> SegmentError {
    match segment.finish() {
        Ok(report) => {
            if let Err(error) = observer.segment_events(report.number, report.events) {
                tracing::error!(exception.message = %error, "segment.report_failed");
            }
        }
        Err(close_error) => {
            tracing::error!(exception.message = %close_error, "segment.close_failed");
        }
    }
    open_error
}

/// The CLI's observer: one status line per second until the run's end, a line per device
/// change and permission check, and power events from the sleep watch.
struct StatusPrinter {
    /// The run ends here, counted in awake time: `Instant` stops while the Mac sleeps.
    ends_at: Instant,
    next_status: Duration,
    power: mpsc::Receiver<PowerEvent>,
}

#[derive(Serialize)]
struct DeviceLine<'a> {
    kind: LineKind,
    segment: u32,
    stem: Stem,
    old_device: Option<&'a str>,
    new_device: &'a str,
    rate_hz: u32,
    cause: DeviceChangeCause,
}

#[derive(Serialize)]
struct MicUnavailableLine<'a> {
    kind: LineKind,
    segment: u32,
    old_device: Option<&'a str>,
    cause: Option<DeviceChangeCause>,
    message: &'a str,
}

#[derive(Serialize)]
struct PermissionLine {
    kind: LineKind,
    service: PermissionService,
    state: PermissionState,
}

#[derive(Serialize)]
struct PowerLine {
    kind: LineKind,
    /// The segment the sleep closed.
    segment: Option<u32>,
}

impl StatusPrinter {
    /// Waits until the Mac wakes. Another sleep before that is allowed at once, since no
    /// segment is open. False when the power watch is gone.
    fn wait_for_wake(&self) -> bool {
        loop {
            match self.power.recv() {
                Ok(PowerEvent::Woke) => return true,
                Ok(PowerEvent::WillSleep(ack)) => ack.allow(),
                Err(mpsc::RecvError) => return false,
            }
        }
    }
}

impl SegmentObserver for StatusPrinter {
    fn stem_started(&mut self, _segment: u32, _start: &StemStart) -> anyhow::Result<()> {
        Ok(())
    }

    fn segment_events(&mut self, _segment: u32, events: Vec<SegmentEvent>) -> anyhow::Result<()> {
        for event in events {
            print_line(&event)?;
        }
        Ok(())
    }

    fn device_changed(&mut self, segment: u32, change: &DeviceChange) -> anyhow::Result<()> {
        print_line(&DeviceLine {
            kind: LineKind::DeviceChanged,
            segment,
            stem: change.stem,
            old_device: change.old_device.as_deref(),
            new_device: &change.new_device,
            rate_hz: change.rate_hz,
            cause: change.cause,
        })
    }

    fn mic_unavailable(
        &mut self,
        segment: u32,
        old_device: Option<&str>,
        cause: Option<DeviceChangeCause>,
        message: &str,
    ) -> anyhow::Result<()> {
        print_line(&MicUnavailableLine {
            kind: LineKind::MicUnavailable,
            segment,
            old_device,
            cause,
            message,
        })
    }

    fn permission_checked(
        &mut self,
        service: PermissionService,
        state: PermissionState,
    ) -> anyhow::Result<()> {
        print_line(&PermissionLine {
            kind: LineKind::Permission,
            service,
            state,
        })
    }

    fn after_drain(&mut self, live: &mut LiveSegment<'_>) -> anyhow::Result<LoopControl> {
        let elapsed = live.elapsed();
        if elapsed >= self.next_status {
            self.next_status += Duration::from_secs(1);
            print_line(&StatusLine {
                kind: LineKind::Status,
                elapsed_s: elapsed.as_secs_f64(),
                segment: live.number(),
                mic: live.status_line(Stem::Mic),
                remote: live.status_line(Stem::Remote),
            })?;
        }
        match self.power.try_recv() {
            Ok(PowerEvent::WillSleep(ack)) => return Ok(LoopControl::Sleep(ack)),
            Ok(PowerEvent::Woke) => return Ok(LoopControl::Woke),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {}
        }
        if Instant::now() >= self.ends_at {
            return Ok(LoopControl::Stop);
        }
        Ok(LoopControl::Continue)
    }

    fn sleep_block_failed(&mut self, _error: &CaptureError) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Starts the listeners a recording process needs: device changes, and sleep and wake sent to
/// `on_power`. Debug builds also start the injected timetable.
pub(crate) fn watch_devices_and_power(
    on_power: impl Fn(PowerEvent) + Clone + Send + Sync + 'static,
) -> Result<(), CaptureError> {
    #[cfg(debug_assertions)]
    crate::inject::start(on_power.clone());
    let devices = capture::watch_hal_changes();
    let power = capture::watch_system_power(on_power);
    if let (Err(_), Err(error)) = (&devices, &power) {
        tracing::warn!(exception.message = %error, "power.watch_failed");
    }
    devices.and(power)
}

/// Records the requested segment, or the folder's next one, for a fixed length of awake time.
/// A sleep closes the segment; the next one opens on wake.
pub(crate) fn record(request: &RecordRequest) -> anyhow::Result<()> {
    let folder = request.folder.as_path();
    let mut number = match request.segment {
        Some(number) => number,
        None => stem_writer::next_segment_number(folder)?,
    };
    let call_id = request.call_id.clone().unwrap_or_else(|| {
        folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let (power_sender, power) = mpsc::channel();
    let on_power = move |event| {
        // The receiver is gone only when the process is ending.
        let _unheard = power_sender.send(event);
    };
    if let Err(error) = watch_devices_and_power(on_power) {
        tracing::warn!(exception.message = %error, "watch.failed");
    }
    let mut printer = StatusPrinter {
        ends_at: Instant::now() + request.length,
        next_status: Duration::from_secs(1),
        power,
    };
    // The run's first segment is the call's start; segments after a wake are mid-call.
    let mut mic_moment = MicMoment::CallStart;
    let mut lid_muted_uid = None;
    loop {
        printer.next_status = Duration::from_secs(1);
        let finished = record_segment(
            &SegmentRequest {
                folder,
                number,
                call_id: &call_id,
                platform: request.platform,
                mic: request.mic,
                mic_moment,
                lid_muted_uid: lid_muted_uid.take(),
            },
            &mut printer,
        )?;
        mic_moment = MicMoment::MidCall;
        lid_muted_uid.clone_from(&finished.lid_muted_uid);
        print_summary(folder, &finished)?;
        match finished.end {
            SegmentEnd::Stopped => return Ok(()),
            SegmentEnd::Sleep(ack) => {
                print_line(&PowerLine {
                    kind: LineKind::SystemSleep,
                    segment: Some(finished.report.number),
                })?;
                ack.allow();
                if !printer.wait_for_wake() {
                    return Ok(());
                }
            }
            SegmentEnd::Woke => {}
        }
        print_line(&PowerLine {
            kind: LineKind::SystemWake,
            segment: None,
        })?;
        if Instant::now() >= printer.ends_at {
            return Ok(());
        }
        number = stem_writer::next_segment_number(folder)?;
    }
}

fn print_summary(folder: &Path, finished: &FinishedSegment) -> anyhow::Result<()> {
    for event in &finished.report.events {
        print_line(event)?;
    }
    let summary_of = |kind: Stem, stem: &RecordStem| {
        finished
            .report
            .stems
            .iter()
            .find(|progress| progress.stem == kind)
            .map(|progress| stem.summary(progress.clone(), finished.stop_host_ns))
    };
    print_line(&SummaryLine {
        kind: LineKind::Summary,
        elapsed_s: finished.elapsed.as_secs_f64(),
        folder: folder.display().to_string(),
        segment: finished.report.number,
        mic: summary_of(Stem::Mic, &finished.mic),
        remote: finished
            .remote
            .as_ref()
            .and_then(|stem| summary_of(Stem::Remote, stem)),
    })
}
