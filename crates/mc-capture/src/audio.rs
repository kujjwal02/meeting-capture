//! `mc-capture audio`: the process the app supervises. It reads commands from stdin and writes
//! reports to stdout, one JSON line each, in the `capture-protocol` shapes. stdout carries
//! nothing else; logs go to stderr. Closing stdin stops the segment, then the process exits.
//! Closing stdin stops within 1 s once its devices are open.

use std::io::{BufRead as _, Write as _};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context as _;
use capture::{CaptureError, Stem};
use capture_protocol::{AudioCommand, AudioErrorKind, AudioReport};
use stem_writer::SegmentEvent;

use crate::MicChoice;
use crate::record::{LiveSegment, SegmentError, SegmentObserver, SegmentRequest, StemStart};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Numbers and writes report lines. The stdin thread and the recording loop share it.
#[derive(Clone)]
struct ReportWriter {
    next_seq: Arc<Mutex<u64>>,
}

impl ReportWriter {
    fn new() -> Self {
        Self {
            next_seq: Arc::new(Mutex::new(1)),
        }
    }

    fn send(&self, report: &AudioReport) -> anyhow::Result<()> {
        // The lock spans the write, so lines leave in seq order.
        let mut next_seq = self.next_seq.lock().unwrap_or_else(PoisonError::into_inner);
        let line = capture_protocol::encode(*next_seq, report)?;
        *next_seq += 1;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{line}").context("write a report to stdout")?;
        stdout.flush().context("flush stdout")?;
        Ok(())
    }

    fn error(&self, kind: AudioErrorKind, message: String) -> anyhow::Result<()> {
        tracing::error!(error.type = %kind, exception.message = message, "audio.error");
        self.send(&AudioReport::Error { kind, message })
    }
}

/// Serves commands until stdin closes. Returns an error, and so exits non-zero, when a segment
/// fails: the app then restarts the process with the next segment number.
pub(crate) fn serve(mic: MicChoice) -> anyhow::Result<()> {
    let writer = ReportWriter::new();
    let (sender, commands) = mpsc::channel();
    let reader_writer = writer.clone();
    std::thread::Builder::new()
        .name("stdin".to_owned())
        .spawn(move || read_commands(&sender, &reader_writer))
        .context("start the stdin thread")?;
    tracing::info!(process.pid = std::process::id(), "audio.ready");
    loop {
        // A closed stdin with no segment open: the app is done with this process.
        let Ok(command) = commands.recv() else {
            return Ok(());
        };
        match command {
            AudioCommand::Stop => writer.error(
                AudioErrorKind::BadCommand,
                "stop arrived with no segment open".to_owned(),
            )?,
            AudioCommand::Start {
                call_id,
                folder,
                segment,
                platform,
            } => {
                let mut observer = AudioObserver {
                    writer: &writer,
                    commands: &commands,
                    next_heartbeat: HEARTBEAT_INTERVAL,
                    is_stdin_closed: false,
                };
                let request = SegmentRequest {
                    folder: &folder,
                    number: segment,
                    call_id: &call_id,
                    platform,
                    mic,
                };
                match crate::record::record_segment(&request, &mut observer) {
                    Ok(finished) => {
                        let number = finished.report.number;
                        let reported = observer
                            .segment_events(number, finished.report.events)
                            .and_then(|()| writer.send(&AudioReport::Stopped { segment: number }));
                        if observer.is_stdin_closed {
                            // An app that quit closed stdin and stdout together.
                            if let Err(error) = reported {
                                tracing::info!(exception.message = %error, "stop.unreported");
                            }
                            return Ok(());
                        }
                        reported?;
                    }
                    Err(error) => {
                        let kind = match &error {
                            SegmentError::Start(_) => AudioErrorKind::SegmentStartFailed,
                            SegmentError::Open(_) => AudioErrorKind::DeviceOpenFailed,
                            SegmentError::Write(_) => AudioErrorKind::WriteFailed,
                            SegmentError::Close(_) => AudioErrorKind::SegmentCloseFailed,
                            // No one reads stdout any more.
                            SegmentError::Report(_) => return Err(error.into()),
                        };
                        // The exit code tells the app too, so a failed report adds nothing.
                        let _unheard = writer.error(kind, error.to_string());
                        return Err(error.into());
                    }
                }
            }
        }
    }
}

/// Parses each stdin line once. A bad line is reported and skipped. The sender drops at EOF,
/// which is how the recording loop hears that stdin closed.
fn read_commands(sender: &mpsc::Sender<AudioCommand>, writer: &ReportWriter) {
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(exception.message = %error, "stdin.read_failed");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match capture_protocol::decode::<AudioCommand>(&line) {
            Ok(command) => {
                tracing::info!(message.id = command.seq, "audio.command");
                if sender.send(command.message).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _unheard = writer.error(
                    AudioErrorKind::BadCommand,
                    format!("not a valid command: {error}"),
                );
            }
        }
    }
    tracing::info!("stdin.closed");
}

struct AudioObserver<'a> {
    writer: &'a ReportWriter,
    commands: &'a Receiver<AudioCommand>,
    next_heartbeat: Duration,
    is_stdin_closed: bool,
}

impl SegmentObserver for AudioObserver<'_> {
    fn stem_started(&mut self, segment: u32, start: &StemStart) -> anyhow::Result<()> {
        self.writer.send(&AudioReport::Recording {
            segment,
            stream: start.stem,
            device: start.device.clone(),
            rate_hz: start.rate_hz,
            first_host_time_ns: start.first_host_time_ns,
        })
    }

    fn segment_events(&mut self, segment: u32, events: Vec<SegmentEvent>) -> anyhow::Result<()> {
        for event in events {
            match event {
                SegmentEvent::Gap { stem, gap } => self.writer.send(&AudioReport::Gap {
                    segment,
                    stream: stem,
                    start_ms: gap.start_ms,
                    start_host_time_ns: gap.start_host_time_ns,
                    length_ms: gap.length_ms,
                    cause: gap.cause,
                })?,
                SegmentEvent::StemStopped { stem, reason } => self
                    .writer
                    .error(AudioErrorKind::StemStopped, format!("{stem}: {reason}"))?,
                SegmentEvent::CheckpointFailed { reason } => self
                    .writer
                    .error(AudioErrorKind::CheckpointFailed, reason)?,
                SegmentEvent::Rotated {
                    from_number,
                    to_number,
                } => self.writer.send(&AudioReport::Rotated {
                    from_segment: from_number,
                    to_segment: to_number,
                })?,
            }
        }
        Ok(())
    }

    fn after_drain(&mut self, live: &mut LiveSegment<'_>) -> anyhow::Result<bool> {
        if live.elapsed() >= self.next_heartbeat {
            self.next_heartbeat += HEARTBEAT_INTERVAL;
            self.writer.send(&AudioReport::Heartbeat {
                segment: live.number(),
                mic: live.counters(Stem::Mic),
                remote: live.counters(Stem::Remote),
            })?;
        }
        match self.commands.try_recv() {
            Ok(AudioCommand::Stop) => Ok(true),
            Ok(AudioCommand::Start { segment, .. }) => {
                self.writer.error(
                    AudioErrorKind::BadCommand,
                    format!(
                        "start of segment {segment} arrived while segment {} is open",
                        live.number()
                    ),
                )?;
                Ok(false)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => {
                self.is_stdin_closed = true;
                Ok(true)
            }
        }
    }

    fn sleep_block_failed(&mut self, error: &CaptureError) -> anyhow::Result<()> {
        self.writer
            .error(AudioErrorKind::PowerAssertionFailed, error.to_string())
    }
}
