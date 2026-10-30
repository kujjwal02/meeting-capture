//! Records one segment of a call folder through the stem writer: 16 kHz stems placed by host
//! time, and `audio.json`. Prints one status line per second, one line per writer event, and a
//! summary at the end.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use capture::{BufferStamp, CallAudioSource, Stem, StemLosses, StemReader};
use serde::Serialize;
use stem_writer::{Segment, SegmentEvent, StemProgress};

use crate::{LineKind, MicChoice};

/// Drain often enough that a 65,536-sample ring at 48 kHz (1.4 s) never fills.
const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct RecordRequest {
    pub folder: PathBuf,
    pub segment: Option<u32>,
    pub call_id: Option<String>,
    pub platform: sources::Platform,
    pub length: Duration,
    pub mic: MicChoice,
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
        }
    }

    /// Moves everything queued in the ring into the segment. Adds the stem's file on the first
    /// drain, when the device's name and rate are known. A stem whose file cannot be added stops
    /// alone, with a `stem_stopped` line; the other stem keeps recording.
    fn drain(&mut self, segment: &mut Segment, reader: &mut StemReader) -> anyhow::Result<()> {
        if !self.is_stopped && !segment.has_stem(self.stem) {
            let format = reader.format();
            if let Err(error) =
                segment.add_stem(self.stem, &format.device_name, format.sample_rate_hz)
            {
                self.is_stopped = true;
                tracing::error!(capture.stem = %self.stem, exception.message = %error, "stem.add_failed");
                print_events(vec![SegmentEvent::StemStopped {
                    stem: self.stem,
                    reason: error.to_string(),
                }])?;
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
        if !self.is_stopped {
            segment.note_ring_losses(self.stem, self.losses);
            segment.write(self.stem, &self.stamps, &self.samples)?;
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

fn print_events(events: Vec<SegmentEvent>) -> anyhow::Result<()> {
    for event in events {
        println!("{}", serde_json::to_string(&event)?);
    }
    Ok(())
}

/// Starts the requested segment, or the folder's next one.
fn start_segment(request: &RecordRequest) -> anyhow::Result<Segment> {
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
    Segment::start(
        folder,
        number,
        &call_id,
        request.platform,
        Box::new(capture::utc_anchor),
    )
    .with_context(|| format!("start segment {number} in {}", folder.display()))
}

pub(crate) fn record(request: &RecordRequest) -> anyhow::Result<()> {
    let folder = request.folder.as_path();
    let mut segment = start_segment(request)?;

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
        (Err(error), _) => return Err(close_after_failed_open(segment, error.into())),
        (Ok(opened), Err(error)) => {
            drop(opened);
            return Err(close_after_failed_open(segment, error));
        }
    };
    let mut mic_reader = readers.mic.reader;
    let mut remote = readers
        .remote
        .map(|opened| (opened.reader, RecordStem::new(Stem::Remote)));

    let started = Instant::now();
    let recording = record_until(
        &mut segment,
        request.length,
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
    let report = finished?;
    recording?;
    last_drain?;
    print_events(report.events)?;

    let summary_of = |kind: Stem, stem: &RecordStem| {
        report
            .stems
            .iter()
            .find(|progress| progress.stem == kind)
            .map(|progress| stem.summary(progress.clone(), stop_host_ns))
    };
    let summary = SummaryLine {
        kind: LineKind::Summary,
        elapsed_s: started.elapsed().as_secs_f64(),
        folder: folder.display().to_string(),
        segment: report.number,
        mic: summary_of(Stem::Mic, &mic_stem),
        remote: remote
            .as_ref()
            .and_then(|(_, stem)| summary_of(Stem::Remote, stem)),
    };
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}

/// Drains both stems every 50 ms and prints one status line per second, until `length` has
/// passed. The caller finishes the segment whatever this returns.
fn record_until(
    segment: &mut Segment,
    length: Duration,
    (mic_stem, mic_reader): (&mut RecordStem, &mut StemReader),
    mut remote: Option<(&mut RecordStem, &mut StemReader)>,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut next_status = started + Duration::from_secs(1);
    loop {
        mic_stem.drain(segment, mic_reader)?;
        if let Some((stem, reader)) = remote.as_mut() {
            stem.drain(segment, reader)?;
        }
        segment.checkpoint_if_due();
        print_events(segment.take_events())?;
        if Instant::now() >= next_status {
            next_status += Duration::from_secs(1);
            let status = StatusLine {
                kind: LineKind::Status,
                elapsed_s: started.elapsed().as_secs_f64(),
                segment: segment.number(),
                mic: segment
                    .take_progress(Stem::Mic)
                    .map(|progress| mic_stem.status_line(progress)),
                remote: remote.as_ref().and_then(|(stem, _)| {
                    segment
                        .take_progress(Stem::Remote)
                        .map(|progress| stem.status_line(progress))
                }),
            };
            println!("{}", serde_json::to_string(&status)?);
        }
        if started.elapsed() >= length {
            return Ok(());
        }
        std::thread::sleep(DRAIN_INTERVAL);
    }
}

/// Closes a segment whose devices did not all open, with whatever the mic wrote, so no empty
/// open segment is left behind. Returns the open error; a failed close is logged beside it.
fn close_after_failed_open(segment: Segment, open_error: anyhow::Error) -> anyhow::Error {
    let closed = segment
        .finish()
        .map_err(anyhow::Error::from)
        .and_then(|report| print_events(report.events));
    if let Err(close_error) = closed {
        tracing::error!(exception.message = %close_error, "segment.close_failed");
    }
    open_error
}
