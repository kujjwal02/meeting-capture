//! Records the stems at native rates and prints one status line per second, to measure what the
//! devices deliver.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use capture::{MicRule, MicSource, StemReader};
use serde::Serialize;

use crate::{MicChoice, Sources};

#[derive(Serialize)]
struct StemStatus {
    device: String,
    device_channels: u16,
    opened_rate_hz: u32,
    samples: u64,
    dropped_samples: u64,
    /// Samples per second of wall clock since the stem opened.
    measured_rate_hz: f64,
    peak_last_second: f32,
}

#[derive(Serialize)]
struct ProbeStatus {
    elapsed_s: f64,
    default_input: Option<String>,
    default_output: Option<String>,
    tap_rate_hz: Option<f64>,
    mic: Option<StemStatus>,
    remote: Option<StemStatus>,
}

struct StemFile {
    reader: StemReader,
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    samples: u64,
    peak: f32,
}

impl StemFile {
    fn create(reader: StemReader, path: &Path) -> anyhow::Result<Self> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: reader.format().sample_rate_hz,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(path, spec)
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Self {
            reader,
            writer,
            samples: 0,
            peak: 0.0,
        })
    }

    fn drain(&mut self) -> anyhow::Result<()> {
        let mut write_result = Ok(());
        let writer = &mut self.writer;
        let samples = &mut self.samples;
        let peak = &mut self.peak;
        self.reader.drain(|chunk| {
            for &sample in chunk {
                *peak = peak.max(sample.abs());
                if write_result.is_ok() {
                    write_result = writer.write_sample(sample);
                }
            }
            *samples += chunk.len() as u64;
        });
        Ok(write_result?)
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts stay far below 2^52"
    )]
    fn status(&mut self, elapsed: Duration) -> StemStatus {
        let format = self.reader.format();
        let status = StemStatus {
            device: format.device_name.clone(),
            device_channels: format.device_channels,
            opened_rate_hz: format.sample_rate_hz,
            samples: self.samples,
            dropped_samples: self.reader.dropped_samples(),
            measured_rate_hz: self.samples as f64 / elapsed.as_secs_f64().max(0.001),
            peak_last_second: self.peak,
        };
        self.peak = 0.0;
        status
    }
}

pub(crate) fn probe(
    length: Duration,
    out: &Path,
    sources: Sources,
    mic: MicChoice,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    let rule = match mic {
        MicChoice::FollowDefault => MicRule::FollowDefault,
        MicChoice::AvoidBluetooth => MicRule::AvoidBluetooth,
    };

    // The tap opens first: an open can block on a permission prompt, and a mic already running
    // would overflow its ring while it waits.
    let mut remote_stem = None;
    let mut remote_source = None;
    if sources != Sources::Mic {
        let opening = Instant::now();
        let (source, reader) = capture::RemoteSource::start()?;
        tracing::info!(open_ms = opening.elapsed().as_millis(), "tap.opened");
        remote_source = Some(source);
        remote_stem = Some((
            Instant::now(),
            StemFile::create(reader, &out.join("remote-native.wav"))?,
        ));
    }

    let mut mic_stem = None;
    let mut mic_source = None;
    if sources != Sources::Remote {
        let opening = Instant::now();
        let (source, reader) = MicSource::start(rule)?;
        tracing::info!(open_ms = opening.elapsed().as_millis(), "mic.opened");
        mic_source = Some(source);
        mic_stem = Some((
            Instant::now(),
            StemFile::create(reader, &out.join("mic-native.wav"))?,
        ));
    }
    let started = Instant::now();
    let mut next_status = started + Duration::from_secs(1);
    while started.elapsed() < length {
        std::thread::sleep(Duration::from_millis(100));
        for (_, stem) in mic_stem.iter_mut().chain(remote_stem.iter_mut()) {
            stem.drain()?;
        }
        if Instant::now() >= next_status {
            next_status += Duration::from_secs(1);
            let devices = capture::default_devices();
            let status = ProbeStatus {
                elapsed_s: started.elapsed().as_secs_f64(),
                default_input: devices.default_input,
                default_output: devices.default_output,
                tap_rate_hz: remote_source
                    .as_ref()
                    .and_then(capture::RemoteSource::current_sample_rate_hz),
                mic: mic_stem
                    .as_mut()
                    .map(|(opened, stem)| stem.status(opened.elapsed())),
                remote: remote_stem
                    .as_mut()
                    .map(|(opened, stem)| stem.status(opened.elapsed())),
            };
            println!("{}", serde_json::to_string(&status)?);
        }
    }

    drop(mic_source);
    drop(remote_source);
    for (_, mut stem) in mic_stem.into_iter().chain(remote_stem) {
        stem.drain()?;
        stem.writer.finalize()?;
    }
    Ok(())
}
