//! The capture rules: whether the mic, the call's sound, the audio helper and the window video
//! record. They read the app's lines for the helpers' reports and the level rows.

use jiff::{SignedDuration, Timestamp};
use telemetry::events;
use telemetry::parsed::{Entry, Line, MetricRow};

use crate::context::CallContext;
use crate::problem::Problem;
use crate::reading;
use crate::{Detector, DetectorKind};

/// The level the stem writer gives a second of zeros: JSON has no minus infinity.
const DIGITAL_SILENCE_DBFS: f64 = -120.0;
/// Matches the helper's `MIC_ZERO_LIMIT`, after which it reads the mic permission.
const MIC_SILENCE_LIMIT: SignedDuration = SignedDuration::from_secs(10);
/// Matches the helper's `REMOTE_ZERO_LIMIT`.
const REMOTE_SILENCE_LIMIT: SignedDuration = SignedDuration::from_mins(1);
/// Remote silence counts only when the user spoke this long in it: in a call where nobody
/// talks, a silent tap is right.
const SPEECH_IN_REMOTE_SILENCE: f64 = 10.0;
/// A routine tap rebuild (a rate or format change on every output switch) is a problem only
/// when the new tap does not record within this long.
const ROUTINE_REBUILD_LIMIT: SignedDuration = SignedDuration::from_secs(5);
/// A wake with no new audio segment for this long is a problem. The app answers a wake with
/// `start` within a second or two.
const RESUME_LIMIT: SignedDuration = SignedDuration::from_secs(20);
/// A helper the supervisor calls failing stays down until it records this long with no
/// failure. The supervisor calls a helper failing at 3 failures in 60 s, and clears that at its
/// next recording, so a helper that records for a few seconds and crashes would clear the
/// problem at each restart and never last the 30 s a notice needs. 60 s is the supervisor's
/// failure window: a helper that records that long is out of the loop by its own measure.
const STEADY_RECORDING: SignedDuration = SignedDuration::from_mins(1);

const DENIED: &str = "denied";
const MIC: &str = "mic";
const REMOTE: &str = "remote";

fn stem_of(line: &Line) -> Option<&str> {
    reading::text(line, events::AUDIO_STREAM)
}

/// Whether a `permission` line says `service` is denied. `None` for another service.
fn permission_refusal(line: &Line, service: &str) -> Option<bool> {
    (reading::text(line, events::PERMISSION_SERVICE)? == service)
        .then(|| reading::text(line, events::PERMISSION_STATE) == Some(DENIED))
}

fn metric_row(line: &Line) -> Option<&MetricRow> {
    match &line.entry {
        Entry::Metric(row) => Some(row),
        Entry::Resource(_)
        | Entry::Dump { .. }
        | Entry::Log(_)
        | Entry::Span(_)
        | Entry::Env(_) => None,
    }
}

fn is_capture_paused(context: &CallContext<'_>) -> bool {
    context.facts.is_stopped_by_user || context.facts.is_asleep
}

/// A run of rows whose stem read digital silence.
#[derive(Debug, Clone, Copy)]
struct SilentRun {
    since: Timestamp,
    /// The user's speech in the run, from `mc.audio.self_speaking`.
    speech_s: f64,
    /// The run lasted its limit, and the speech it needs.
    is_over_limit: bool,
}

/// Extends `run` with one row's peak. A row without the stem's point leaves it as it is.
fn extend_run(
    run: &mut Option<SilentRun>,
    peak_dbfs: Option<f64>,
    row: &MetricRow,
    at: Timestamp,
    limit: SignedDuration,
    speech_needed_s: f64,
    context: &CallContext<'_>,
) {
    let Some(peak_dbfs) = peak_dbfs else {
        return;
    };
    if peak_dbfs > DIGITAL_SILENCE_DBFS {
        *run = None;
        return;
    }
    let speech_s = reading::point(row, events::AUDIO_SELF_SPEAKING).unwrap_or_default()
        * reading::row_seconds(row);
    let run = run.get_or_insert(SilentRun {
        since: at,
        speech_s: 0.0,
        is_over_limit: false,
    });
    run.speech_s += speech_s;
    run.is_over_limit = context.elapsed(run.since, at) >= limit && run.speech_s >= speech_needed_s;
}

/// The mic: its permission, its stream, and its samples.
pub struct Mic {
    is_permission_refused: bool,
    is_lost: bool,
    silence: Option<SilentRun>,
}

impl Mic {
    #[must_use]
    pub fn new() -> Self {
        Self {
            is_permission_refused: false,
            is_lost: false,
            silence: None,
        }
    }
}

impl Default for Mic {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector for Mic {
    fn kind(&self) -> DetectorKind {
        DetectorKind::Mic
    }

    fn take_line(&mut self, line: &Line, context: &CallContext<'_>) {
        if let Some(row) = metric_row(line) {
            let peak = reading::stem_point(row, events::AUDIO_LEVEL_PEAK, MIC);
            extend_run(
                &mut self.silence,
                peak,
                row,
                line.at,
                MIC_SILENCE_LIMIT,
                0.0,
                context,
            );
            return;
        }
        match reading::event_name(line) {
            Some(events::CAPTURE_AUDIO_PERMISSION) => {
                if let Some(is_refused) = permission_refusal(line, "microphone") {
                    self.is_permission_refused = is_refused;
                }
            }
            Some(events::CAPTURE_AUDIO_STREAM_LOST) if stem_of(line) == Some(MIC) => {
                self.is_lost = true;
            }
            Some(events::CAPTURE_AUDIO_MIC_UNAVAILABLE) => self.is_lost = true,
            Some(events::CAPTURE_AUDIO_DEVICE_CHANGED | events::CAPTURE_AUDIO_RECORDING)
                if stem_of(line) == Some(MIC) =>
            {
                self.is_lost = false;
                self.silence = None;
            }
            // Rows stop over a sleep; the stream after it is a new one.
            Some(events::CAPTURE_AUDIO_SLEEP) => self.silence = None,
            Some(_) | None => {}
        }
    }

    fn advance(&mut self, _at: Timestamp, _context: &CallContext<'_>) {}

    fn next_deadline(&self, _context: &CallContext<'_>) -> Option<Timestamp> {
        None
    }

    fn problem(&self, context: &CallContext<'_>) -> Option<Problem> {
        if is_capture_paused(context) {
            None
        } else if self.is_permission_refused {
            Some(Problem::MicPermissionRefused)
        } else if self.is_lost {
            Some(Problem::MicLost)
        } else if self.silence.is_some_and(|run| run.is_over_limit) {
            Some(Problem::MicDigitalSilence)
        } else {
            None
        }
    }
}

/// A tap that stopped, and whether its cause is routine.
#[derive(Debug, Clone, Copy)]
struct RemoteLoss {
    since: Timestamp,
    is_routine: bool,
}

/// The call's sound: the tap's permission, its stream, and its samples.
pub struct RemoteAudio {
    is_permission_refused: bool,
    loss: Option<RemoteLoss>,
    silence: Option<SilentRun>,
    now: Timestamp,
}

impl RemoteAudio {
    #[must_use]
    pub fn new(opened_at: Timestamp) -> Self {
        Self {
            is_permission_refused: false,
            loss: None,
            silence: None,
            now: opened_at,
        }
    }
}

/// Whether a rebuild cause comes with ordinary device switches.
fn is_routine_rebuild(cause: Option<&str>) -> bool {
    matches!(cause, Some("tap_format_changed" | "tap_rate_mismatch"))
}

impl Detector for RemoteAudio {
    fn kind(&self) -> DetectorKind {
        DetectorKind::RemoteAudio
    }

    fn take_line(&mut self, line: &Line, context: &CallContext<'_>) {
        self.now = self.now.max(line.at);
        if let Some(row) = metric_row(line) {
            let peak = reading::stem_point(row, events::AUDIO_LEVEL_PEAK, REMOTE);
            extend_run(
                &mut self.silence,
                peak,
                row,
                line.at,
                REMOTE_SILENCE_LIMIT,
                SPEECH_IN_REMOTE_SILENCE,
                context,
            );
            return;
        }
        match reading::event_name(line) {
            Some(events::CAPTURE_AUDIO_PERMISSION) => {
                if let Some(is_refused) = permission_refusal(line, "audio_capture") {
                    self.is_permission_refused = is_refused;
                }
            }
            Some(events::CAPTURE_AUDIO_STREAM_LOST) if stem_of(line) == Some(REMOTE) => {
                let is_routine =
                    is_routine_rebuild(reading::text(line, events::AUDIO_CHANGE_CAUSE));
                // A routine rebuild inside a loss does not make the loss routine.
                let is_routine = is_routine && self.loss.is_none_or(|loss| loss.is_routine);
                let since = self.loss.map_or(self.now, |loss| loss.since);
                self.loss = Some(RemoteLoss { since, is_routine });
            }
            Some(events::CAPTURE_AUDIO_DEVICE_CHANGED | events::CAPTURE_AUDIO_RECORDING)
                if stem_of(line) == Some(REMOTE) =>
            {
                self.loss = None;
                self.silence = None;
            }
            Some(events::CAPTURE_AUDIO_SLEEP) => self.silence = None,
            Some(_) | None => {}
        }
    }

    fn advance(&mut self, at: Timestamp, _context: &CallContext<'_>) {
        self.now = self.now.max(at);
    }

    fn next_deadline(&self, context: &CallContext<'_>) -> Option<Timestamp> {
        let loss = self.loss.filter(|loss| loss.is_routine)?;
        Some(context.deadline(loss.since, ROUTINE_REBUILD_LIMIT)).filter(|at| *at > self.now)
    }

    fn problem(&self, context: &CallContext<'_>) -> Option<Problem> {
        let is_lost = self.loss.is_some_and(|loss| {
            !loss.is_routine || context.elapsed(loss.since, self.now) >= ROUTINE_REBUILD_LIMIT
        });
        if is_capture_paused(context) {
            None
        } else if self.is_permission_refused {
            Some(Problem::AudioCapturePermissionRefused)
        } else if is_lost {
            Some(Problem::RemoteLost)
        } else if self.silence.is_some_and(|run| run.is_over_limit) {
            Some(Problem::RemoteDigitalSilence)
        } else {
            None
        }
    }
}

/// Which helper a `capture.process_failed` line is about.
fn is_failed_helper(line: &Line, helper: &str) -> bool {
    reading::text(line, events::CAPTURE_HELPER) == Some(helper)
}

/// Whether a helper process runs, from its `capture.process_failed` and recording lines.
#[derive(Debug, Clone, Copy, Default)]
struct HelperRuns {
    /// A failure came after the last recording.
    is_down: bool,
    /// The supervisor called the helper failing at its last failure.
    is_crash_looping: bool,
    /// The first recording after the last failure.
    recording_since: Option<Timestamp>,
}

impl HelperRuns {
    fn take_failure(&mut self, line: &Line) {
        self.is_down = true;
        self.is_crash_looping = reading::flag(line, events::CAPTURE_IS_FAILING) == Some(true);
        self.recording_since = None;
    }

    fn take_recording(&mut self, at: Timestamp) {
        if self.is_down {
            self.is_down = false;
            self.recording_since = Some(at);
        }
    }

    fn is_down(&self, now: Timestamp, context: &CallContext<'_>) -> bool {
        self.is_down
            || self
                .steady_deadline(context)
                .is_some_and(|deadline| now < deadline)
    }

    /// When a crash-looping helper's recording has lasted long enough to count.
    fn steady_deadline(&self, context: &CallContext<'_>) -> Option<Timestamp> {
        let since = self.recording_since.filter(|_| self.is_crash_looping)?;
        Some(context.deadline(since, STEADY_RECORDING))
    }
}

/// The `mc-capture audio` process: whether it runs, and whether it came back after a wake.
pub struct AudioHelper {
    runs: HelperRuns,
    woke_at: Option<Timestamp>,
    now: Timestamp,
}

impl AudioHelper {
    #[must_use]
    pub fn new(opened_at: Timestamp) -> Self {
        Self {
            runs: HelperRuns::default(),
            woke_at: None,
            now: opened_at,
        }
    }
}

impl Detector for AudioHelper {
    fn kind(&self) -> DetectorKind {
        DetectorKind::AudioHelper
    }

    fn take_line(&mut self, line: &Line, _context: &CallContext<'_>) {
        self.now = self.now.max(line.at);
        match reading::event_name(line) {
            Some(events::CAPTURE_PROCESS_FAILED) if is_failed_helper(line, "audio") => {
                self.runs.take_failure(line);
            }
            Some(events::CAPTURE_AUDIO_RECORDING) => {
                self.runs.take_recording(self.now);
                self.woke_at = None;
            }
            Some(events::CAPTURE_AUDIO_WAKE) => self.woke_at = Some(self.now),
            Some(events::CAPTURE_AUDIO_SLEEP) => self.woke_at = None,
            Some(_) | None => {}
        }
    }

    fn advance(&mut self, at: Timestamp, _context: &CallContext<'_>) {
        self.now = self.now.max(at);
    }

    fn next_deadline(&self, context: &CallContext<'_>) -> Option<Timestamp> {
        let resume_deadline = self
            .woke_at
            .map(|woke_at| context.deadline(woke_at, RESUME_LIMIT));
        resume_deadline
            .into_iter()
            .chain(self.runs.steady_deadline(context))
            .filter(|at| *at > self.now)
            .min()
    }

    fn problem(&self, context: &CallContext<'_>) -> Option<Problem> {
        let is_not_resumed = self
            .woke_at
            .is_some_and(|woke_at| context.elapsed(woke_at, self.now) >= RESUME_LIMIT);
        if is_capture_paused(context) {
            None
        } else if self.runs.is_down(self.now, context) {
            Some(Problem::AudioHelperDown)
        } else if is_not_resumed {
            Some(Problem::AudioNotResumed)
        } else {
            None
        }
    }
}

/// The window video: its permission, its process and its stream.
pub struct Video {
    is_permission_refused: bool,
    runs: HelperRuns,
    is_stream_lost: bool,
    now: Timestamp,
}

impl Video {
    #[must_use]
    pub fn new(opened_at: Timestamp) -> Self {
        Self {
            is_permission_refused: false,
            runs: HelperRuns::default(),
            is_stream_lost: false,
            now: opened_at,
        }
    }
}

impl Detector for Video {
    fn kind(&self) -> DetectorKind {
        DetectorKind::Video
    }

    fn take_line(&mut self, line: &Line, _context: &CallContext<'_>) {
        self.now = self.now.max(line.at);
        match reading::event_name(line) {
            Some(events::CAPTURE_VIDEO_ERROR)
                if reading::text(line, events::ERROR_TYPE) == Some("permission_denied") =>
            {
                self.is_permission_refused = true;
            }
            Some(events::CAPTURE_VIDEO_PERMISSION) => {
                if permission_refusal(line, "screen_recording") == Some(true) {
                    self.is_permission_refused = true;
                }
            }
            Some(events::CAPTURE_PROCESS_FAILED) if is_failed_helper(line, "video") => {
                self.runs.take_failure(line);
            }
            Some(events::CAPTURE_VIDEO_STREAM_LOST) => self.is_stream_lost = true,
            Some(events::CAPTURE_VIDEO_GAP)
                if reading::text(line, events::ERROR_TYPE) == Some("stream_restart") =>
            {
                self.is_stream_lost = false;
            }
            Some(events::CAPTURE_VIDEO_RECORDING) => {
                self.is_permission_refused = false;
                self.runs.take_recording(self.now);
                self.is_stream_lost = false;
            }
            Some(_) | None => {}
        }
    }

    fn advance(&mut self, at: Timestamp, _context: &CallContext<'_>) {
        self.now = self.now.max(at);
    }

    fn next_deadline(&self, context: &CallContext<'_>) -> Option<Timestamp> {
        self.runs
            .steady_deadline(context)
            .filter(|at| *at > self.now)
    }

    fn problem(&self, context: &CallContext<'_>) -> Option<Problem> {
        if is_capture_paused(context) {
            None
        } else if self.is_permission_refused {
            Some(Problem::ScreenRecordingRefused)
        } else if self.runs.is_down(self.now, context) {
            Some(Problem::VideoHelperDown)
        } else if self.is_stream_lost {
            Some(Problem::VideoStreamLost)
        } else {
            None
        }
    }
}
