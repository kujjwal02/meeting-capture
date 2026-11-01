//! `mc-capture audio`: the process the app supervises. It reads commands from stdin and writes
//! reports to stdout, one JSON line each, in the `capture-protocol` shapes. stdout carries
//! nothing else; logs go to stderr. Closing stdin stops the segment, then the process exits.
//! Closing stdin stops within 1 s once its devices are open. A sleep closes the segment and
//! reports `system_sleep`; on wake the process reports `system_wake` and waits for the app's
//! next `start`, since the app owns the call's segment numbers.

use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use capture::{CaptureError, PermissionService, PermissionState, PowerEvent, Stem};
use capture_protocol::{AudioCommand, AudioErrorKind, AudioReport, DeviceChangeCause};
use stem_writer::SegmentEvent;
use telemetry::DumpReason;
use telemetry::debug_mode::DebugMode;

use crate::MicChoice;
use crate::record::{
    DeviceChange, LiveSegment, LoopControl, SegmentEnd, SegmentError, SegmentObserver,
    SegmentRequest, StemStart,
};

/// Each heartbeat carries one second of stem levels, so the app's 1 s metric rows need no
/// second message kind.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// What reaches the serving loop: commands from stdin, its close, and power events.
enum HelperInput {
    Command(AudioCommand),
    StdinClosed,
    Power(PowerEvent),
}

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

/// Permission states this process reported, so a check that finds nothing new stays quiet.
#[derive(Default)]
struct ReportedPermissions {
    states: HashMap<PermissionService, PermissionState>,
}

impl ReportedPermissions {
    fn report(
        &mut self,
        writer: &ReportWriter,
        service: PermissionService,
        state: PermissionState,
    ) -> anyhow::Result<()> {
        if self.states.insert(service, state) == Some(state) {
            return Ok(());
        }
        writer.send(&AudioReport::Permission { service, state })
    }
}

/// Serves commands until stdin closes. Returns an error, and so exits non-zero, when a segment
/// fails: the app then restarts the process with the next segment number.
pub(crate) fn serve(mic: MicChoice) -> anyhow::Result<()> {
    let served = serve_commands(mic);
    // The process ends when this returns, and a dump cut off there loses its last lines.
    join_recorder_dumps();
    served
}

fn serve_commands(mic: MicChoice) -> anyhow::Result<()> {
    let writer = ReportWriter::new();
    let (sender, inputs) = mpsc::channel();
    let power_sender = sender.clone();
    let on_power = move |event| {
        // The receiver is gone only when the process is ending.
        let _unheard = power_sender.send(HelperInput::Power(event));
    };
    if let Err(error) = crate::record::watch_devices_and_power(on_power) {
        writer.error(AudioErrorKind::WatchFailed, error.to_string())?;
    }
    let reader_writer = writer.clone();
    std::thread::Builder::new()
        .name("stdin".to_owned())
        .spawn(move || read_commands(&sender, &reader_writer))
        .context("start the stdin thread")?;
    tracing::info!(process.pid = std::process::id(), "audio.ready");
    let mut permissions = ReportedPermissions::default();
    let mut idle = IdleInputs::default();
    // The built-in mic a closed lid muted, carried to the call's next segment.
    let mut lid_muted_uid = None;
    loop {
        let Some(command) = idle.next_command(&inputs, &writer)? else {
            return Ok(());
        };
        match command {
            // A stop cancels a start that waits for the wake.
            AudioCommand::Stop if idle.deferred_start.take().is_some() => {
                tracing::info!("start.cancelled");
            }
            AudioCommand::Stop => writer.error(
                AudioErrorKind::BadCommand,
                "stop arrived with no segment open".to_owned(),
            )?,
            AudioCommand::DumpRecorder { path } => spawn_recorder_dump(path),
            AudioCommand::SetDebug { until } => follow_debug_mode(until.as_deref()),
            AudioCommand::Start {
                call_id,
                folder,
                segment,
                platform,
                mic_moment,
            } => {
                let mut observer = AudioObserver {
                    writer: &writer,
                    inputs: &inputs,
                    permissions: &mut permissions,
                    next_heartbeat: HEARTBEAT_INTERVAL,
                    is_stdin_closed: false,
                };
                let _span = tracing::info_span!(
                    "capture.segment",
                    call.id = %call_id,
                    segment.number = segment
                )
                .entered();
                let request = SegmentRequest {
                    folder: &folder,
                    number: segment,
                    call_id: &call_id,
                    platform,
                    mic,
                    mic_moment,
                    lid_muted_uid: lid_muted_uid.take(),
                };
                match crate::record::record_segment(&request, &mut observer) {
                    Ok(finished) => {
                        let number = finished.report.number;
                        lid_muted_uid.clone_from(&finished.lid_muted_uid);
                        idle.is_asleep = matches!(finished.end, SegmentEnd::Sleep(_));
                        let is_stdin_closed = observer.is_stdin_closed;
                        let reported = observer
                            .segment_events(number, finished.report.events)
                            .and_then(|()| writer.send(&AudioReport::Stopped { segment: number }))
                            .and_then(|()| report_segment_end(&writer, number, finished.end));
                        if is_stdin_closed {
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
                            SegmentError::RebuildLimit { .. } => AudioErrorKind::TapRebuildLimit,
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

/// The serving loop's state between segments. Between a sleep and its wake the devices may be
/// gone, so a start waits for the wake.
#[derive(Default)]
struct IdleInputs {
    is_asleep: bool,
    deferred_start: Option<AudioCommand>,
}

impl IdleInputs {
    /// The next command to run, after handling power events. `None` once stdin closed.
    fn next_command(
        &mut self,
        inputs: &Receiver<HelperInput>,
        writer: &ReportWriter,
    ) -> anyhow::Result<Option<AudioCommand>> {
        loop {
            if !self.is_asleep
                && let Some(start) = self.deferred_start.take()
            {
                return Ok(Some(start));
            }
            // The power senders live as long as the process, so this never disconnects.
            let Ok(input) = inputs.recv() else {
                return Ok(None);
            };
            match input {
                HelperInput::Command(start @ AudioCommand::Start { .. }) if self.is_asleep => {
                    tracing::info!("start.deferred_until_wake");
                    self.deferred_start = Some(start);
                }
                HelperInput::Command(command) => return Ok(Some(command)),
                // A closed stdin with no segment open: the app is done with this process.
                HelperInput::StdinClosed => return Ok(None),
                HelperInput::Power(PowerEvent::WillSleep(ack)) => {
                    tracing::info!("power.will_sleep");
                    self.is_asleep = true;
                    let reported = writer.send(&AudioReport::SystemSleep { segment: None });
                    ack.allow();
                    reported?;
                }
                HelperInput::Power(PowerEvent::Woke) => {
                    tracing::info!("power.woke");
                    self.is_asleep = false;
                    writer.send(&AudioReport::SystemWake)?;
                }
            }
        }
    }
}

/// Reports why a closed segment ended, after its `stopped`. A sleep is allowed only once the
/// app has heard that the segment closed.
fn report_segment_end(writer: &ReportWriter, number: u32, end: SegmentEnd) -> anyhow::Result<()> {
    match end {
        SegmentEnd::Stopped => Ok(()),
        SegmentEnd::Sleep(ack) => {
            let reported = writer.send(&AudioReport::SystemSleep {
                segment: Some(number),
            });
            ack.allow();
            reported
        }
        SegmentEnd::Woke => writer.send(&AudioReport::SystemWake),
    }
}

/// Sets the log file level from the app's `set_debug`. A value that is not a time turns debug
/// mode off: the app wrote it, so the next change corrects it.
fn follow_debug_mode(until: Option<&str>) {
    let mode =
        DebugMode::from_enabled_until(until, jiff::Timestamp::now()).unwrap_or_else(|error| {
            tracing::warn!(exception.message = %error, "set_debug.bad_until");
            DebugMode::Off
        });
    if let Some(switch) = crate::DEBUG_MODE_SWITCH.get() {
        switch.apply_now(mode);
    }
}

/// The dump threads still running, joined before the process ends.
static RECORDER_DUMPS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Dumps the flight recorder on its own thread: a dump writes up to 8 MB, which would stall the
/// drain loop past what the rings hold.
fn spawn_recorder_dump(path: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("recorder-dump".to_owned())
        .spawn(
            move || match telemetry::dump_recorder(&path, DumpReason::AppRequest) {
                Ok(line_count) => tracing::info!(
                    file.path = %path.display(),
                    mc.recorder.line_count = line_count,
                    "recorder.dumped"
                ),
                Err(error) => tracing::warn!(
                    file.path = %path.display(),
                    exception.message = %error,
                    "recorder.dump_failed"
                ),
            },
        );
    match spawned {
        Ok(dump) => {
            let mut dumps = RECORDER_DUMPS
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            dumps.retain(|running| !running.is_finished());
            dumps.push(dump);
        }
        Err(error) => {
            tracing::warn!(exception.message = %error, "recorder.dump_thread_failed");
        }
    }
}

fn join_recorder_dumps() {
    let dumps = std::mem::take(
        &mut *RECORDER_DUMPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    for dump in dumps {
        if dump.join().is_err() {
            tracing::warn!("recorder.dump_panicked");
        }
    }
}

/// Parses each stdin line once. A bad line is reported and skipped. At EOF it sends
/// `StdinClosed`, which is how the recording loop hears that stdin closed.
fn read_commands(sender: &mpsc::Sender<HelperInput>, writer: &ReportWriter) {
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
                if sender.send(HelperInput::Command(command.message)).is_err() {
                    return;
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
    let _unheard = sender.send(HelperInput::StdinClosed);
}

struct AudioObserver<'a> {
    writer: &'a ReportWriter,
    inputs: &'a Receiver<HelperInput>,
    permissions: &'a mut ReportedPermissions,
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
            mic_route: start.mic_route.clone(),
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

    fn device_changed(&mut self, segment: u32, change: &DeviceChange) -> anyhow::Result<()> {
        self.writer.send(&AudioReport::DeviceChanged {
            segment,
            stream: change.stem,
            old_device: change.old_device.clone(),
            new_device: change.new_device.clone(),
            rate_hz: change.rate_hz,
            cause: change.cause,
            mic_route: change.mic_route.clone(),
        })
    }

    fn mic_unavailable(
        &mut self,
        segment: u32,
        old_device: Option<&str>,
        cause: Option<DeviceChangeCause>,
        message: &str,
    ) -> anyhow::Result<()> {
        self.writer.send(&AudioReport::MicUnavailable {
            segment,
            old_device: old_device.map(str::to_owned),
            cause,
            message: message.to_owned(),
        })
    }

    fn permission_checked(
        &mut self,
        service: PermissionService,
        state: PermissionState,
    ) -> anyhow::Result<()> {
        self.permissions.report(self.writer, service, state)
    }

    fn after_drain(&mut self, live: &mut LiveSegment<'_>) -> anyhow::Result<LoopControl> {
        if live.elapsed() >= self.next_heartbeat {
            self.next_heartbeat += HEARTBEAT_INTERVAL;
            self.writer.send(&AudioReport::Heartbeat {
                segment: live.number(),
                mic: live.counters(Stem::Mic),
                remote: live.counters(Stem::Remote),
                mic_level: live.level(Stem::Mic),
                remote_level: live.level(Stem::Remote),
            })?;
        }
        match self.inputs.try_recv() {
            Ok(HelperInput::Command(AudioCommand::Stop)) => Ok(LoopControl::Stop),
            Ok(HelperInput::Command(AudioCommand::Start { segment, .. })) => {
                self.writer.error(
                    AudioErrorKind::BadCommand,
                    format!(
                        "start of segment {segment} arrived while segment {} is open",
                        live.number()
                    ),
                )?;
                Ok(LoopControl::Continue)
            }
            Ok(HelperInput::Command(AudioCommand::DumpRecorder { path })) => {
                spawn_recorder_dump(path);
                Ok(LoopControl::Continue)
            }
            Ok(HelperInput::Command(AudioCommand::SetDebug { until })) => {
                follow_debug_mode(until.as_deref());
                Ok(LoopControl::Continue)
            }
            Ok(HelperInput::Power(PowerEvent::WillSleep(ack))) => {
                tracing::info!(segment = live.number(), "power.will_sleep");
                Ok(LoopControl::Sleep(ack))
            }
            Ok(HelperInput::Power(PowerEvent::Woke)) => {
                tracing::warn!(segment = live.number(), "power.woke_unannounced");
                Ok(LoopControl::Woke)
            }
            Err(TryRecvError::Empty) => Ok(LoopControl::Continue),
            Ok(HelperInput::StdinClosed) | Err(TryRecvError::Disconnected) => {
                self.is_stdin_closed = true;
                Ok(LoopControl::Stop)
            }
        }
    }

    fn sleep_block_failed(&mut self, error: &CaptureError) -> anyhow::Result<()> {
        self.writer
            .error(AudioErrorKind::PowerAssertionFailed, error.to_string())
    }
}
