//! Runs one case: a debug helper with the case's faults, answered as the app's supervisor
//! answers it, and the detectors fed once a second as the app's status board feeds them.
//!
//! The answers follow `Supervisor::take_line` and `ProcessSupervisor::run` in
//! `crates/app/src/call_capture.rs`: `start` at launch, `start` for the next segment at a
//! `system_wake` with no segment open, a new process 5 s after `permission_denied`, and
//! `capture.process_failed` with a new process 1 s after any other exit.

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use capture_protocol::{
    AudioCommand, AudioReport, Line as WireLine, MicMoment, Platform, StemLevel, VideoCommand,
    VideoErrorKind, VideoReport, WindowHint,
};
use detectors::{Change, Edge, Host, LineQueue, TraceKey};
use jiff::Timestamp;
use serde_json::Value;
use telemetry::metrics::{MetricsScope, RowPoints};
use telemetry::trace::TraceId;

use crate::cases::{CASE_LENGTH, Case, Helper, timetable_from};

const TICK_INTERVAL: Duration = Duration::from_secs(1);
const STOP_WAIT: Duration = Duration::from_secs(10);
/// The supervisor's wait before a new Screen Recording check.
const REFUSED_RESPAWN_GAP: Duration = Duration::from_secs(5);
/// The supervisor's first restart delay.
const FAILED_RESPAWN_GAP: Duration = Duration::from_secs(1);
const LEVEL_ROW_INTERVAL_MS: u32 = 1000;

/// What the whole run shares.
pub(crate) struct Setup {
    pub helper_path: PathBuf,
    pub out: PathBuf,
    pub lines: Arc<LineQueue>,
    pub host: Host,
    /// A Finder window for the video cases.
    pub window_hint: Option<WindowHint>,
}

/// Where one case writes.
pub(crate) struct CasePaths {
    pub call_id: String,
    pub trace: TraceKey,
    pub call_folder: PathBuf,
    pub debug_folder: PathBuf,
    pub replay_folder: PathBuf,
}

impl CasePaths {
    pub(crate) fn of(out: &Path, case: &Case) -> Self {
        // A path that never exists: the trace comes from it, as from a call log's path.
        let call_id = out
            .join("calls")
            .join(format!("{}.jsonl", case.name))
            .display()
            .to_string();
        Self {
            trace: TraceKey(TraceId::for_call(&call_id).to_string()),
            call_id,
            call_folder: out.join("calls").join(case.name),
            debug_folder: out.join("debug").join(case.name),
            replay_folder: out.join("replay").join(case.name),
        }
    }
}

/// When the case ran, and what its detectors said live.
pub(crate) struct CaseRun {
    pub started_at: Timestamp,
    pub edges: Vec<Edge>,
}

enum HelperEvent {
    Report(String),
    Exited,
}

/// One helper process and its stdin.
struct HelperProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<HelperEvent>,
    seq: u64,
    /// The driver closed stdin, so an exit is expected.
    is_stopping: bool,
}

pub(crate) fn run_case(setup: &mut Setup, case: &Case) -> anyhow::Result<CaseRun> {
    let paths = CasePaths::of(&setup.out, case);
    for folder in [
        &paths.call_folder,
        &paths.debug_folder,
        &paths.replay_folder,
    ] {
        std::fs::create_dir_all(folder)
            .with_context(|| format!("could not create {}", folder.display()))?;
    }
    let scope = MetricsScope::open_rows(&paths.replay_folder, LEVEL_ROW_INTERVAL_MS)
        .with_trace_id(TraceId::for_call(&paths.call_id));
    let started = Instant::now();
    let started_at = Timestamp::now();
    let mut driver = CaseDriver {
        case,
        paths: &paths,
        scope: &scope,
        started,
        segment: 1,
        respawn_at: None,
        edges: Vec::new(),
    };
    let mut process = driver.spawn(setup)?;
    driver.send_start(setup, &mut process)?;
    let mut next_tick = started + TICK_INTERVAL;
    while started.elapsed() < CASE_LENGTH {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match process.events.recv_timeout(wait) {
            Ok(HelperEvent::Report(text)) => driver.take_report(setup, &mut process, &text)?,
            Ok(HelperEvent::Exited) => driver.take_exit(&mut process),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        if driver.respawn_at.is_some_and(|at| Instant::now() >= at) {
            driver.respawn_at = None;
            driver.stop(&mut process);
            process = driver.spawn(setup)?;
            driver.segment += 1;
            driver.send_start(setup, &mut process)?;
        }
        if Instant::now() >= next_tick {
            next_tick += TICK_INTERVAL;
            driver.tick(setup, &mut process);
        }
    }
    driver.stop(&mut process);
    driver.tick(setup, &mut process);
    let edges = driver.edges;
    setup.host.forget(&paths.trace);
    drop(scope);
    Ok(CaseRun { started_at, edges })
}

struct CaseDriver<'a> {
    case: &'a Case,
    paths: &'a CasePaths,
    scope: &'a MetricsScope,
    started: Instant,
    segment: u32,
    respawn_at: Option<Instant>,
    edges: Vec<Edge>,
}

impl CaseDriver<'_> {
    fn spawn(&self, setup: &Setup) -> anyhow::Result<HelperProcess> {
        let timetable = timetable_from(self.case.timetable, self.started.elapsed().as_secs());
        let logs = setup.out.join("logs");
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs.join("mc-capture-stderr.log"))
            .context("could not open the helper's stderr file")?;
        let mut child = Command::new(&setup.helper_path)
            .arg("--log-dir")
            .arg(&logs)
            .arg("--debug-dir")
            .arg(setup.out.join("debug"))
            .arg(self.case.helper.subcommand())
            .env("MC_CAPTURE_INJECT", &timetable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .with_context(|| format!("could not start {}", setup.helper_path.display()))?;
        let stdout = child.stdout.take().context("the helper has no stdout")?;
        let (sender, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(HelperEvent::Report(line)).is_err() {
                    return;
                }
            }
            let _closed = sender.send(HelperEvent::Exited);
        });
        tracing::info!(
            call.id = self.paths.call_id,
            mc.capture.helper = self.case.helper.subcommand(),
            inject.schedule = timetable,
            "capture check started a helper"
        );
        Ok(HelperProcess {
            stdin: child.stdin.take(),
            child,
            events,
            seq: 0,
            is_stopping: false,
        })
    }

    fn send_start(&self, setup: &Setup, process: &mut HelperProcess) -> anyhow::Result<()> {
        let folder = self.paths.call_folder.clone();
        let call_id = self.paths.call_id.clone();
        match self.case.helper {
            Helper::Audio => process.send(&AudioCommand::Start {
                call_id,
                folder,
                segment: self.segment,
                platform: Platform::Meet,
                mic_moment: if self.segment == 1 {
                    MicMoment::CallStart
                } else {
                    MicMoment::MidCall
                },
            }),
            Helper::Video => process.send(&VideoCommand::Start {
                call_id,
                folder,
                segment: self.segment,
                audio_segment: self.segment,
                platform: Platform::Meet,
                window_hint: setup
                    .window_hint
                    .clone()
                    .context("a video case needs a Finder window")?,
            }),
        }
    }

    fn take_report(
        &mut self,
        setup: &Setup,
        process: &mut HelperProcess,
        text: &str,
    ) -> anyhow::Result<()> {
        match self.case.helper {
            Helper::Audio => {
                let Ok(line) = serde_json::from_str::<WireLine<AudioReport>>(text) else {
                    tracing::warn!(
                        exception.message = text,
                        "the helper sent a line that is not a report"
                    );
                    return Ok(());
                };
                capture_protocol::report_lines::log_audio_report(
                    &self.paths.call_id,
                    &line.message,
                );
                match line.message {
                    AudioReport::Heartbeat {
                        mic_level,
                        remote_level,
                        ..
                    } => self.write_levels(mic_level.as_ref(), remote_level.as_ref()),
                    AudioReport::SystemWake => {
                        if !self.case.is_wake_start_withheld {
                            self.segment += 1;
                            self.send_start(setup, process)?;
                        }
                    }
                    AudioReport::Recording { .. }
                    | AudioReport::Gap { .. }
                    | AudioReport::Rotated { .. }
                    | AudioReport::DeviceChanged { .. }
                    | AudioReport::StreamLost { .. }
                    | AudioReport::MicUnavailable { .. }
                    | AudioReport::Permission { .. }
                    | AudioReport::SystemSleep { .. }
                    | AudioReport::Stopped { .. }
                    | AudioReport::Error { .. } => {}
                }
            }
            Helper::Video => {
                let Ok(line) = serde_json::from_str::<WireLine<VideoReport>>(text) else {
                    tracing::warn!(
                        exception.message = text,
                        "the helper sent a line that is not a report"
                    );
                    return Ok(());
                };
                capture_protocol::report_lines::log_video_report(
                    &self.paths.call_id,
                    &line.message,
                );
                if let VideoReport::Error {
                    kind: VideoErrorKind::PermissionDenied,
                    ..
                } = line.message
                {
                    // The refused process waits with no segment open until the new one starts.
                    self.respawn_at = Some(Instant::now() + REFUSED_RESPAWN_GAP);
                }
            }
        }
        Ok(())
    }

    fn take_exit(&mut self, process: &mut HelperProcess) {
        let status = process.child.wait();
        if process.is_stopping {
            return;
        }
        let reason = match status {
            Ok(status) => format!("the helper exited: {status}"),
            Err(error) => format!("could not read the helper's exit: {error}"),
        };
        tracing::warn!(
            event.name = "capture.process_failed",
            call.id = self.paths.call_id,
            mc.capture.helper = self.case.helper.subcommand(),
            error.type = "capture_process_failed",
            exception.message = reason,
            "a capture process failed; restarting"
        );
        process.is_stopping = true;
        self.respawn_at = Some(Instant::now() + FAILED_RESPAWN_GAP);
    }

    /// The levels as the app writes them: one row per heartbeat, stamped when it arrives.
    fn write_levels(&self, mic: Option<&StemLevel>, remote: Option<&StemLevel>) {
        let mut points = RowPoints::default();
        for (stem, level) in [("mic", mic), ("remote", remote)] {
            let Some(level) = level else { continue };
            let attributes = [("mc.audio.stem", Value::from(stem))];
            points.gauge("mc.audio.level.rms", "dBFS", &attributes, level.rms_dbfs);
            points.gauge("mc.audio.level.peak", "dBFS", &attributes, level.peak_dbfs);
        }
        if !points.is_empty() {
            self.scope.write_row(Timestamp::now(), &points);
        }
    }

    /// Feeds the detectors the lines since the last tick, logs their verdicts, and dumps every
    /// recorder at a raise.
    fn tick(&mut self, setup: &mut Setup, process: &mut HelperProcess) {
        let (raw_lines, dropped_count) = setup.lines.drain();
        if dropped_count > 0 {
            tracing::warn!(
                mc.detectors.dropped_lines = dropped_count,
                "the detector queue dropped lines"
            );
        }
        let mut lines: Vec<_> = raw_lines
            .iter()
            .filter_map(|raw| telemetry::parsed::parse_line(raw).ok())
            .collect();
        lines.sort_by_key(|line| line.at);
        for line in &lines {
            setup.host.take_line(line);
        }
        setup.host.advance_to(Timestamp::now());
        let output = setup.host.drain_output();
        for edge in output
            .edges
            .iter()
            .filter(|edge| edge.call == self.paths.trace)
        {
            detectors::problem_lines::log_edge(edge, &self.paths.call_id, None);
            match edge.change {
                Change::Raised(_) | Change::Changed { .. } => self.dump_recorders(process),
                Change::Cleared(_) => {}
            }
            self.edges.push(edge.clone());
        }
        for notice in output
            .notices
            .iter()
            .filter(|notice| notice.call == self.paths.trace)
        {
            detectors::problem_lines::log_notice(notice.notice, &self.paths.call_id);
        }
    }

    fn stop(&self, process: &mut HelperProcess) {
        let is_running = matches!(process.child.try_wait(), Ok(None));
        if is_running && process.stdin.is_some() {
            let sent = match self.case.helper {
                Helper::Audio => process.send(&AudioCommand::Stop),
                Helper::Video => process.send(&VideoCommand::Stop),
            };
            if let Err(error) = sent {
                tracing::warn!(exception.message = %error, "could not send stop to the helper");
            }
        }
        process.wait_for_exit();
    }

    fn dump_recorders(&self, process: &mut HelperProcess) {
        if let Err(error) = telemetry::dump_recorder_into(
            &self.paths.debug_folder,
            telemetry::DumpReason::CallProblem,
        ) {
            tracing::warn!(exception.message = %error, "could not dump the check's recorder");
        }
        let path = self.paths.debug_folder.join(format!(
            "recorder-mc-capture-{}.jsonl",
            self.case.helper.subcommand()
        ));
        let sent = match self.case.helper {
            Helper::Audio => process.send(&AudioCommand::DumpRecorder { path }),
            Helper::Video => process.send(&VideoCommand::DumpRecorder { path }),
        };
        if let Err(error) = sent {
            tracing::warn!(exception.message = %error, "could not ask the helper for its recorder");
        }
    }
}

impl HelperProcess {
    fn send<M: serde::Serialize>(&mut self, message: &M) -> anyhow::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .context("the helper's stdin is closed")?;
        self.seq += 1;
        let line = serde_json::to_string(&WireLine {
            seq: self.seq,
            message,
        })?;
        writeln!(stdin, "{line}")?;
        stdin.flush()?;
        Ok(())
    }

    fn close_stdin(&mut self) {
        self.is_stopping = true;
        self.stdin = None;
    }

    /// Closes stdin and waits for the exit; kills a helper that hangs.
    fn wait_for_exit(&mut self) {
        self.close_stdin();
        let deadline = Instant::now() + STOP_WAIT;
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        tracing::warn!("the helper did not exit after stop; killing it");
        let _killed = self.child.kill();
        let _reaped = self.child.wait();
    }
}
