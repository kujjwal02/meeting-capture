//! Records one segment of a call folder through the stem writer: 16 kHz stems placed by host
//! time, and `audio.json`. `record_segment` is the loop both `record` and `audio` run; each
//! passes a `SegmentObserver` that reports in its own format and says when to stop.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use capture::{
    BufferStamp, CallAudioSource, CaptureError, IdleSleepBlock, Stem, StemLosses, StemReader,
};
use serde::Serialize;
use stem_writer::{Segment, SegmentEvent, SegmentReport, StemProgress};

use crate::{LineKind, MicChoice};

/// Drain often enough that a 65,536-sample ring at 48 kHz (1.4 s) never fills. Also bounds how
/// long a stop waits before the segment closes.
const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

/// What `pmset -g assertions` names while stems are open.
const SLEEP_BLOCK_REASON: &str = "Meeting Capture is recording a call";

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
    /// Runs after each drain step. `Ok(true)` closes the segment.
    fn after_drain(&mut self, live: &mut LiveSegment<'_>) -> anyhow::Result<bool>;
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
    pub stop_host_ns: u64,
    pub elapsed: Duration,
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
    losses: StemLosses,
    dropped_samples_at_start: Option<u64>,
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
            dropped_samples_at_start: None,
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
        reader.drain(
            |stamp| stamps.push(stamp),
            |chunk| samples.extend_from_slice(chunk),
        );
        self.losses = reader.losses();
        self.dropped_samples_at_start
            .get_or_insert(self.losses.dropped_samples);
        if self.is_stopped {
            return Ok(());
        }
        if !self.is_start_reported
            && let Some(first) = self.stamps.first()
        {
            self.is_start_reported = true;
            self.pending_start = Some(StemStart {
                stem: self.stem,
                device: reader.format().device_name.clone(),
                rate_hz: first.sample_rate_hz,
                first_host_time_ns: first.host_time_ns,
            });
        }
        segment.note_ring_losses(self.stem, self.losses);
        segment.write(self.stem, &self.stamps, &self.samples)?;
        Ok(())
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
        Self::line(progress, peak, self.losses)
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
        let totals = Self::line(progress, peak, self.losses);
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

/// Records one segment until the observer says stop: opens the mic, then the tap, drains both
/// every 50 ms, then closes the segment in teardown order. Idle sleep stays off while the
/// stems are open.
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
            observer
                .sleep_block_failed(&error)
                .map_err(SegmentError::Report)?;
            None
        }
    };

    let mut mic_stem = RecordStem::new(Stem::Mic);
    let mut drain_outcome = Ok(());
    let opened = CallAudioSource::open(request.mic.rule(), |mic_reader| {
        if drain_outcome.is_ok() {
            drain_outcome = mic_stem.drain(&mut segment, mic_reader);
            segment.checkpoint_if_due();
        }
    });
    let (mut source, readers) = match (opened, drain_outcome) {
        (Ok(opened), Ok(())) => opened,
        (Err(error), _) => {
            return Err(close_after_failed_open(
                segment,
                SegmentError::Open(error.into()),
                observer,
            ));
        }
        (Ok(opened), Err(error)) => {
            drop(opened);
            return Err(close_after_failed_open(
                segment,
                SegmentError::Write(error),
                observer,
            ));
        }
    };
    let mut mic_reader = readers.mic.reader;
    let mut remote = readers
        .remote
        .map(|opened| (opened.reader, RecordStem::new(Stem::Remote)));

    let started = Instant::now();
    let recording = record_until_stopped(
        &mut segment,
        started,
        observer,
        (&mut mic_stem, &mut mic_reader),
        remote.as_mut().map(|(reader, stem)| (stem, reader)),
    );

    // Teardown order: no callback runs after this, so the last drain holds every sample.
    source.stop_callbacks();
    let stop_host_ns = capture::host_now_ns();
    let last_drain = mic_stem
        .drain(&mut segment, &mut mic_reader)
        .and_then(|()| match remote.as_mut() {
            Some((reader, stem)) => stem.drain(&mut segment, reader),
            None => Ok(()),
        });
    let finished = segment.finish();
    drop(source);
    drop(sleep_block);
    let report = finished.map_err(|error| SegmentError::Close(error.into()))?;
    recording?;
    last_drain.map_err(SegmentError::Write)?;
    Ok(FinishedSegment {
        report,
        stop_host_ns,
        elapsed: started.elapsed(),
        mic: mic_stem,
        remote: remote.map(|(_, stem)| stem),
    })
}

/// Drains both stems every 50 ms until the observer says stop. The caller finishes the segment
/// whatever this returns.
fn record_until_stopped(
    segment: &mut Segment,
    started: Instant,
    observer: &mut impl SegmentObserver,
    (mic_stem, mic_reader): (&mut RecordStem, &mut StemReader),
    mut remote: Option<(&mut RecordStem, &mut StemReader)>,
) -> Result<(), SegmentError> {
    loop {
        mic_stem
            .drain(segment, mic_reader)
            .map_err(SegmentError::Write)?;
        if let Some((stem, reader)) = remote.as_mut() {
            stem.drain(segment, reader).map_err(SegmentError::Write)?;
        }
        segment.checkpoint_if_due();
        let number = segment.number();
        mic_stem.report(number, observer)?;
        if let Some((stem, _)) = remote.as_mut() {
            stem.report(number, observer)?;
        }
        observer
            .segment_events(number, segment.take_events())
            .map_err(SegmentError::Report)?;
        let mut live = LiveSegment {
            segment: &mut *segment,
            mic: mic_stem,
            remote: remote.as_ref().map(|(stem, _)| &**stem),
            started,
        };
        if observer
            .after_drain(&mut live)
            .map_err(SegmentError::Report)?
        {
            return Ok(());
        }
        std::thread::sleep(DRAIN_INTERVAL);
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

/// The CLI's observer: one status line per second until the length has passed.
struct StatusPrinter {
    length: Duration,
    next_status: Duration,
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

    fn after_drain(&mut self, live: &mut LiveSegment<'_>) -> anyhow::Result<bool> {
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
        Ok(elapsed >= self.length)
    }

    fn sleep_block_failed(&mut self, _error: &CaptureError) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Records the requested segment, or the folder's next one, for a fixed length.
pub(crate) fn record(request: &RecordRequest) -> anyhow::Result<()> {
    let folder = request.folder.as_path();
    let number = match request.segment {
        Some(number) => number,
        None => stem_writer::next_segment_number(folder)?,
    };
    let call_id = request.call_id.clone().unwrap_or_else(|| {
        folder
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let mut printer = StatusPrinter {
        length: request.length,
        next_status: Duration::from_secs(1),
    };
    let finished = record_segment(
        &SegmentRequest {
            folder,
            number,
            call_id: &call_id,
            platform: request.platform,
            mic: request.mic,
        },
        &mut printer,
    )?;
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
