//! Records the stems at native rates and prints one status line per second, to measure what the
//! devices deliver. The last line sums up each stem.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use capture::{BufferStamp, CallAudioSource, MicRequest, OpenedStem, StemReader};
use serde::Serialize;

use crate::{LineKind, MicChoice};

type WavFile = hound::WavWriter<std::io::BufWriter<std::fs::File>>;

#[derive(Serialize)]
struct StemStatus {
    device: String,
    device_channels: u16,
    opened_rate_hz: u32,
    /// The rate on the newest stamp.
    stamp_rate_hz: Option<u32>,
    samples: u64,
    dropped_samples: u64,
    dropped_stamps: u64,
    untimed_buffers: u64,
    /// Samples per second of wall clock since the stem opened.
    measured_rate_hz: f64,
    peak_last_second: f32,
    first_host_time_ns: Option<u64>,
    /// Host clock now, minus the host time just past the newest stamped buffer, at the last drain.
    lag_ms: Option<f64>,
}

#[derive(Serialize)]
struct ProbeStatus {
    kind: LineKind,
    elapsed_s: f64,
    default_input: Option<String>,
    default_output: Option<String>,
    tap_rate_hz: Option<u32>,
    mic: StemStatus,
    remote: Option<StemStatus>,
}

#[derive(Serialize)]
struct StemSummary {
    open_ms: u128,
    /// Samples dropped by the time every stem was open.
    dropped_samples_at_start: u64,
    peak: f32,
    lag_ms_min: Option<f64>,
    lag_ms_max: Option<f64>,
    #[serde(flatten)]
    totals: StemStatus,
}

#[derive(Serialize)]
struct ProbeSummary {
    kind: LineKind,
    elapsed_s: f64,
    aggregate_uid: Option<&'static str>,
    /// The remote stem's first host time minus the mic's.
    first_stamp_offset_ms: Option<f64>,
    mic: StemSummary,
    remote: Option<StemSummary>,
}

/// One stem's WAV file and the measurements taken while draining into it. The file opens on the
/// first drain, when the reader's rate is known.
struct StemFile {
    path: PathBuf,
    writer: Option<WavFile>,
    opened: Instant,
    samples: u64,
    peak_last_second: f32,
    peak: f32,
    first_stamp: Option<BufferStamp>,
    last_stamp: Option<BufferStamp>,
    lag_ms: Option<f64>,
    lag_ms_min: Option<f64>,
    lag_ms_max: Option<f64>,
}

impl StemFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            writer: None,
            opened: Instant::now(),
            samples: 0,
            peak_last_second: 0.0,
            peak: 0.0,
            first_stamp: None,
            last_stamp: None,
            lag_ms: None,
            lag_ms_min: None,
            lag_ms_max: None,
        }
    }

    fn drain(&mut self, reader: &mut StemReader) -> anyhow::Result<()> {
        if self.writer.is_none() {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: reader.format().sample_rate_hz,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let writer = hound::WavWriter::create(&self.path, spec)
                .with_context(|| format!("create {}", self.path.display()))?;
            self.writer = Some(writer);
        }
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        let mut write_outcome = Ok(());
        let samples = &mut self.samples;
        let peak_last_second = &mut self.peak_last_second;
        let first_stamp = &mut self.first_stamp;
        let last_stamp = &mut self.last_stamp;
        reader.drain(
            |stamp| {
                first_stamp.get_or_insert(stamp);
                *last_stamp = Some(stamp);
            },
            |chunk| {
                for &sample in chunk {
                    *peak_last_second = peak_last_second.max(sample.abs());
                    if write_outcome.is_ok() {
                        write_outcome = writer.write_sample(sample);
                    }
                }
                *samples += chunk.len() as u64;
            },
        );
        self.measure_lag();
        Ok(write_outcome?)
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "nanosecond spans between two nearby host times stay far below 2^52"
    )]
    fn measure_lag(&mut self) {
        let Some(last) = self.last_stamp else {
            return;
        };
        let buffer_ns =
            u64::from(last.frame_count) * 1_000_000_000 / u64::from(last.sample_rate_hz.max(1));
        let buffer_end_ns = last.host_time_ns + buffer_ns;
        let now_ns = capture::host_now_ns();
        let lag_ms = if now_ns >= buffer_end_ns {
            (now_ns - buffer_end_ns) as f64 / 1e6
        } else {
            -((buffer_end_ns - now_ns) as f64 / 1e6)
        };
        self.lag_ms = Some(lag_ms);
        self.lag_ms_min = Some(self.lag_ms_min.map_or(lag_ms, |min| min.min(lag_ms)));
        self.lag_ms_max = Some(self.lag_ms_max.map_or(lag_ms, |max| max.max(lag_ms)));
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts stay far below 2^52"
    )]
    fn status(&mut self, reader: &StemReader) -> StemStatus {
        let format = reader.format();
        let losses = reader.losses();
        let status = StemStatus {
            device: format.device_name.clone(),
            device_channels: format.device_channels,
            opened_rate_hz: format.sample_rate_hz,
            stamp_rate_hz: self.last_stamp.map(|stamp| stamp.sample_rate_hz),
            samples: self.samples,
            dropped_samples: losses.dropped_samples,
            dropped_stamps: losses.dropped_stamps,
            untimed_buffers: losses.untimed_buffers,
            measured_rate_hz: self.samples as f64 / self.opened.elapsed().as_secs_f64().max(0.001),
            peak_last_second: self.peak_last_second,
            first_host_time_ns: self.first_stamp.map(|stamp| stamp.host_time_ns),
            lag_ms: self.lag_ms,
        };
        self.peak = self.peak.max(self.peak_last_second);
        self.peak_last_second = 0.0;
        status
    }
}

/// A stem as the probe runs it: its reader, its file, and what happened while it opened.
struct ProbeStem {
    reader: StemReader,
    file: StemFile,
    open_ms: u128,
    dropped_samples_at_start: u64,
}

impl ProbeStem {
    fn new(opened: OpenedStem, file: StemFile) -> Self {
        Self {
            reader: opened.reader,
            file,
            open_ms: opened.open_duration.as_millis(),
            dropped_samples_at_start: 0,
        }
    }

    fn drain(&mut self) -> anyhow::Result<()> {
        self.file.drain(&mut self.reader)
    }

    fn mark_started(&mut self) {
        self.dropped_samples_at_start = self.reader.losses().dropped_samples;
    }

    fn status(&mut self) -> StemStatus {
        self.file.status(&self.reader)
    }

    fn finish(mut self) -> anyhow::Result<StemSummary> {
        self.drain()?;
        let totals = self.status();
        if let Some(writer) = self.file.writer.take() {
            writer.finalize()?;
        }
        Ok(StemSummary {
            open_ms: self.open_ms,
            dropped_samples_at_start: self.dropped_samples_at_start,
            peak: self.file.peak,
            lag_ms_min: self.file.lag_ms_min,
            lag_ms_max: self.file.lag_ms_max,
            totals,
        })
    }

    fn first_host_time_ns(&self) -> Option<u64> {
        self.file.first_stamp.map(|stamp| stamp.host_time_ns)
    }
}

pub(crate) fn probe(length: Duration, out: &Path, mic: MicChoice) -> anyhow::Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let rule = mic.rule();

    let mut mic_file = StemFile::new(out.join("mic-native.wav"));
    let mut drain_outcome = Ok(());
    let (mut source, readers) =
        CallAudioSource::open(MicRequest::call_start(rule), |mic_reader| {
            if drain_outcome.is_ok() {
                drain_outcome = mic_file.drain(mic_reader);
            }
        })?;
    drain_outcome?;
    let mut mic_stem = ProbeStem::new(readers.mic?, mic_file);
    tracing::info!(open_ms = mic_stem.open_ms, "mic.opened");
    let mut remote_stem = readers
        .remote
        .map(|opened| ProbeStem::new(opened, StemFile::new(out.join("remote-native.wav"))));
    if let Some(remote) = &remote_stem {
        tracing::info!(
            open_ms = remote.open_ms,
            device.uid = source.remote().map(capture::RemoteSource::aggregate_uid),
            "tap.opened"
        );
    }
    for stem in std::iter::once(&mut mic_stem).chain(remote_stem.as_mut()) {
        stem.drain()?;
        stem.mark_started();
    }

    let started = Instant::now();
    let mut next_status = started + Duration::from_secs(1);
    while started.elapsed() < length {
        std::thread::sleep(Duration::from_millis(100));
        for stem in std::iter::once(&mut mic_stem).chain(remote_stem.as_mut()) {
            stem.drain()?;
        }
        if Instant::now() >= next_status {
            next_status += Duration::from_secs(1);
            let devices = capture::default_devices();
            let status = ProbeStatus {
                kind: LineKind::Status,
                elapsed_s: started.elapsed().as_secs_f64(),
                default_input: devices.default_input,
                default_output: devices.default_output,
                tap_rate_hz: source
                    .remote()
                    .map(capture::RemoteSource::current_sample_rate_hz),
                mic: mic_stem.status(),
                remote: remote_stem.as_mut().map(ProbeStem::status),
            };
            println!("{}", serde_json::to_string(&status)?);
        }
    }

    source.stop_callbacks();
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        reason = "two host times a few ms apart"
    )]
    let first_stamp_offset_ms = mic_stem
        .first_host_time_ns()
        .zip(remote_stem.as_ref().and_then(ProbeStem::first_host_time_ns))
        .map(|(mic_ns, remote_ns)| remote_ns.wrapping_sub(mic_ns) as i64 as f64 / 1e6);
    let summary = ProbeSummary {
        kind: LineKind::Summary,
        elapsed_s: started.elapsed().as_secs_f64(),
        aggregate_uid: source.remote().map(capture::RemoteSource::aggregate_uid),
        first_stamp_offset_ms,
        mic: mic_stem.finish()?,
        remote: remote_stem.map(ProbeStem::finish).transpose()?,
    };
    drop(source);
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
