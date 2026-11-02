//! What each call is doing now: waiting for its first caption, recording, or not recording and
//! why, and whether its capture has a problem. Meet calls and Slack huddles both feed it. The
//! rules live in the `detectors` crate and read the app's own telemetry lines; the board feeds
//! them each tick, keeps the caption tally and the titles, and says what to announce. The tray,
//! the notifications and mc-meet only show it.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use detectors::{
    CallNotice, CallSnapshot, CaptionSource, CaptionState, Change, DetectorKind, Edge, Host,
    LineQueue, Notice, OpenProblem, ProblemSpan, TraceKey,
};
use huddle_captions::HuddleHealth;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;
use sources::browser_call::HealthSource;
use tauri::{AppHandle, Manager as _};
use telemetry::parsed::Line;
use telemetry::trace::TraceId;

pub(crate) use detectors::Problem;

use crate::AppState;
use crate::calls::CallPlatform;

const TICK_INTERVAL: Duration = Duration::from_secs(1);
/// The caption problems mc-meet shows.
const KEPT_PROBLEMS: usize = 3;
/// A call whose captions moved on logs `call.captions` at most this often, unless it waits for
/// a caption or has a problem.
const CAPTION_LINE_INTERVAL: Duration = Duration::from_secs(5);
/// A wall clock that moves this much more than the monotonic clock between two ticks means the
/// host was suspended.
const SUSPEND_LIMIT: Duration = Duration::from_secs(5);
/// One detector's problems dump the recorders at most this often per call. A headset that
/// reconnects every minute would otherwise dump every minute.
const DUMP_INTERVAL: Duration = Duration::from_mins(1);
const RECENT_CALLS: usize = 5;

/// An open call's state, from its captions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallState {
    /// The call is open and no caption has arrived yet.
    Waiting,
    /// A caption arrived, and no caption problem is in view.
    Recording,
    Problem(Problem),
    /// The user stopped recording the call. No caption is expected and no problem counts until
    /// they resume.
    Stopped,
}

/// How an ended call went, for the recent calls list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallOutcome {
    Recorded,
    RecordedWithProblems,
    NoTranscript,
}

/// What the tray icon shows: the worst state of any open call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayLook {
    Idle,
    Recording,
    Problem,
}

/// One fact a source reports about an open call that no rule reads: the caption tally, the
/// user's stop and the tab video.
#[derive(Debug, Clone)]
pub(crate) enum Observation {
    /// A Meet caption event. Every version of one utterance has the same `line_key`.
    MeetCaption { line_key: String },
    /// The caption lines a reopened log already holds, by line key. They count, but say nothing
    /// about whether captions arrive now.
    MeetLoggedCaptions(Vec<String>),
    /// Caption lines arrived or changed, and the huddle log holds this many.
    HuddleCaptionLines(u64),
    /// The user stopped recording the call (`true`) or resumed it (`false`).
    RecordingStopped(bool),
    /// The call's Meet tab video wrote a fragment.
    TabVideoRecorded,
}

#[derive(Debug, Clone)]
pub(crate) struct Announced {
    pub platform: CallPlatform,
    pub title: Option<String>,
    pub notice: Notice,
}

/// A call whose problem started at this tick, and whose recorders are due a dump.
#[derive(Debug, Clone)]
pub(crate) struct ProblemStart {
    pub call_id: PathBuf,
    pub platform: CallPlatform,
    pub problem: Problem,
    /// The call's first caption problem in this run.
    pub is_first_caption_problem: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Tick {
    pub announcements: Vec<Announced>,
    pub problem_starts: Vec<ProblemStart>,
    /// Calls that left the recent calls list since the last tick. Nothing reports on them again.
    pub forgotten_calls: Vec<PathBuf>,
}

/// One clock reading, taken by the caller.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Moment {
    /// For durations, which must not jump with the wall clock.
    pub instant: Instant,
    /// For what is shown and stored.
    pub timestamp: Timestamp,
}

impl Moment {
    pub(crate) fn now() -> Self {
        Self {
            instant: Instant::now(),
            timestamp: Timestamp::now(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProblemRecord {
    pub problem: Problem,
    pub at: Timestamp,
}

/// An open call as the tray and mc-meet show it.
#[derive(Debug, Clone)]
pub(crate) struct CallView {
    pub id: PathBuf,
    pub platform: CallPlatform,
    pub title: Option<String>,
    pub state: CallState,
    pub started_at: Timestamp,
    /// The first caption, or the first one after the last problem.
    pub recording_since: Option<Timestamp>,
    pub caption_count: u64,
    pub last_caption_at: Option<Timestamp>,
    pub last_caption_age: Option<Duration>,
    /// Meet only.
    pub caption_source: Option<HealthSource>,
    /// Slack huddles only.
    pub huddle_health: Option<HuddleHealth>,
    /// The last caption problems, oldest first.
    pub problems: Vec<ProblemRecord>,
    /// Capture problems in view, lowest layer first.
    pub capture_problems: Vec<OpenProblem>,
}

/// An ended call for the recent calls list.
#[derive(Debug, Clone)]
pub(crate) struct EndedCallView {
    pub id: PathBuf,
    pub platform: CallPlatform,
    pub title: Option<String>,
    pub started_at: Timestamp,
    pub ended_at: Timestamp,
    pub caption_count: u64,
    pub outcome: CallOutcome,
    /// The call's Meet tab video wrote at least one fragment.
    pub is_tab_video_recorded: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct BoardView {
    /// Open calls, oldest first.
    pub calls: Vec<CallView>,
    /// Ended calls, newest first.
    pub recent: Vec<EndedCallView>,
    pub is_huddle_permission_missing: bool,
}

impl BoardView {
    pub(crate) fn tray_look(&self) -> TrayLook {
        let has_problem = self.calls.iter().any(|call| {
            matches!(call.state, CallState::Problem(_)) || !call.capture_problems.is_empty()
        });
        if self.is_huddle_permission_missing || has_problem {
            TrayLook::Problem
        } else if self
            .calls
            .iter()
            .any(|call| call.state == CallState::Recording)
        {
            TrayLook::Recording
        } else {
            TrayLook::Idle
        }
    }

    /// The call a debug report is for: the newest open call, else the last ended one.
    pub(crate) fn current_or_last_call(&self) -> Option<(PathBuf, CallPlatform)> {
        self.calls
            .iter()
            .max_by_key(|call| call.started_at)
            .map(|call| (call.id.clone(), call.platform))
            .or_else(|| {
                self.recent
                    .first()
                    .map(|call| (call.id.clone(), call.platform))
            })
    }
}

/// The open calls, the last ended ones, and the detectors that judge them.
pub(crate) struct CallBoard {
    board: Mutex<Board>,
    /// The lines the telemetry tap hands over for the detectors.
    lines: Arc<LineQueue>,
}

struct Board {
    open: HashMap<PathBuf, TrackedCall>,
    /// Newest first.
    recent: VecDeque<EndedCall>,
    is_huddle_permission_missing: bool,
    /// Calls that left `recent`, for the next tick.
    forgotten: Vec<PathBuf>,
    host: Host,
    /// When each call's detector last dumped the recorders.
    dumped_at: HashMap<(PathBuf, DetectorKind), Instant>,
    /// A line that fails to parse is logged once per run.
    is_parse_failure_logged: bool,
}

struct TrackedCall {
    id: PathBuf,
    trace: TraceKey,
    platform: CallPlatform,
    title: Option<String>,
    started_at: Timestamp,
    caption_count: u64,
    last_caption: Option<Moment>,
    /// Device and utterance of every Meet caption line, so revisions count once.
    line_keys: HashSet<String>,
    /// "Stop recording this call" was chosen and "Resume recording" was not. A rejoin
    /// (`call.resumed`) keeps it.
    is_stopped_by_user: bool,
    /// The Meet tab video wrote a fragment. A rejoin keeps it.
    is_tab_video_recorded: bool,
    /// A caption problem was raised in this run.
    has_had_caption_problem: bool,
    /// The detectors' caption verdict at the last tick.
    caption_state: CaptionState,
    caption_lines: CaptionLines,
}

/// What the call's `call.captions` lines said so far, so a line goes out only when the count
/// rose or the last caption moved on.
#[derive(Default)]
struct CaptionLines {
    logged_count: u64,
    /// The `mc.caption.last_at` of the last line. A revised caption moves it and not the count.
    logged_last_caption_at: Option<Timestamp>,
    logged_at: Option<Instant>,
    /// The call waited for a caption or had a problem at the last tick, so a rise logs at once.
    /// The first caption ends the wait before the tick sees it.
    was_awaiting_captions: bool,
    /// The caption count of a reopened log's backlog, not logged yet.
    pending_backlog_count: Option<u64>,
}

/// One `call.captions` line, logged after the board lock is released.
struct CaptionsLine {
    call_id: PathBuf,
    platform: CallPlatform,
    caption_count: u64,
    /// `None` for a backlog line: it says nothing about whether captions arrive now.
    last_at: Option<Timestamp>,
    is_backlog: bool,
}

impl CaptionsLine {
    fn log(&self) {
        tracing::info!(
            event.name = "call.captions",
            call.id = %self.call_id.display(),
            call.platform = self.platform.as_str(),
            call.caption_count = self.caption_count,
            mc.caption.last_at = self.last_at.map(|at| at.to_string()),
            mc.caption.is_backlog = self.is_backlog,
            "call captions counted"
        );
    }
}

/// One line about a problem edge or notice, logged after the board lock is released.
enum ProblemLine {
    Edge {
        edge: Edge,
        call_id: String,
        platform: CallPlatform,
    },
    Notice {
        notice: Notice,
        call_id: String,
    },
}

impl ProblemLine {
    fn log(&self) {
        match self {
            Self::Edge {
                edge,
                call_id,
                platform,
            } => detectors::problem_lines::log_edge(edge, call_id, Some(platform.as_str())),
            Self::Notice { notice, call_id } => {
                detectors::problem_lines::log_notice(*notice, call_id);
            }
        }
    }
}

/// One tick's detector output, acted on once the lock is released.
#[derive(Default)]
struct TickOutput {
    tick: Tick,
    problem_lines: Vec<ProblemLine>,
}

struct EndedCall {
    call: TrackedCall,
    ended_at: Timestamp,
}

impl CallBoard {
    pub(crate) fn new(lines: Arc<LineQueue>) -> Self {
        Self {
            board: Mutex::new(Board {
                open: HashMap::new(),
                recent: VecDeque::new(),
                is_huddle_permission_missing: false,
                forgotten: Vec::new(),
                host: Host::new(),
                dumped_at: HashMap::new(),
                is_parse_failure_logged: false,
            }),
            lines,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Board> {
        self.board.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A call opened, or one that ended in this run opened again and keeps its counts.
    /// `is_stopped_by_user` comes from the call's log, so a stop survives a restart. The caller
    /// logs `call.started` or `call.resumed` after this.
    pub(crate) fn open(
        &self,
        id: &Path,
        platform: CallPlatform,
        title: Option<String>,
        is_stopped_by_user: bool,
        now: Moment,
    ) {
        let was_stopped_by_user = {
            let mut board = self.lock();
            if board.open.contains_key(id) {
                return;
            }
            let resumed = board
                .recent
                .iter()
                .position(|ended| ended.call.id == id)
                .and_then(|index| board.recent.remove(index));
            let mut call = match resumed {
                Some(EndedCall { mut call, .. }) => {
                    call.title = title.or(call.title.take());
                    call.caption_state = CaptionState::Waiting;
                    call
                }
                None => TrackedCall::new(id, platform, title, now),
            };
            let was_stopped_by_user = call.is_stopped_by_user;
            call.is_stopped_by_user = is_stopped_by_user;
            board.open.insert(id.to_path_buf(), call);
            was_stopped_by_user
        };
        // The detectors and `mc debug replay` learn a stop only from its line. Before the open
        // line, so no problem is raised between the two.
        if is_stopped_by_user != was_stopped_by_user {
            log_stop_from_call_log(id, is_stopped_by_user);
        }
    }

    /// A call ended. The detectors hear it from the `call.ended` line, so the caption count
    /// goes out first: a call whose last captions were not logged yet would read as silent.
    pub(crate) fn end(&self, id: &Path, now: Moment) {
        let final_line = {
            let mut board = self.lock();
            let Some(mut call) = board.open.remove(id) else {
                return;
            };
            let final_line = call.take_final_caption_line(now);
            tracing::info!(
                event.name = "call.status",
                call.id = %call.id.display(),
                call.caption_count = call.caption_count,
                call.status = "ended",
                "call status changed"
            );
            board.recent.push_front(EndedCall {
                call,
                ended_at: now.timestamp,
            });
            while board.recent.len() > RECENT_CALLS {
                if let Some(dropped) = board.recent.pop_back() {
                    board.forgotten.push(dropped.call.id);
                }
            }
            final_line
        };
        if let Some(line) = final_line {
            line.log();
        }
    }

    pub(crate) fn observe(&self, id: &Path, observation: Observation, now: Moment) {
        let mut board = self.lock();
        if let Some(call) = board.open.get_mut(id) {
            call.observe(observation, now);
        }
    }

    /// Whether the user stopped recording this open call. The one answer the supervisor, the
    /// huddle watcher and the tray all read.
    pub(crate) fn is_stopped_by_user(&self, id: &Path) -> bool {
        self.lock()
            .open
            .get(id)
            .is_some_and(|call| call.is_stopped_by_user)
    }

    /// The open calls the user stopped recording.
    pub(crate) fn stopped_call_ids(&self) -> BTreeSet<PathBuf> {
        self.lock()
            .open
            .values()
            .filter(|call| call.is_stopped_by_user)
            .map(|call| call.id.clone())
            .collect()
    }

    pub(crate) fn set_huddle_permission_missing(&self, is_missing: bool) {
        self.lock().is_huddle_permission_missing = is_missing;
    }

    /// Logs each call's due `call.captions` line, runs the detectors over the lines since the
    /// last tick up to `now`, and returns what to announce and which problems started.
    pub(crate) fn tick(&self, now: Moment) -> Tick {
        let caption_lines: Vec<CaptionsLine> = {
            let mut board = self.lock();
            let mut lines = Vec::new();
            for call in board.open.values_mut() {
                call.take_caption_lines(now, &mut lines);
            }
            lines
        };
        // A logged line reaches the queue at once, so this tick's detectors read it.
        for line in &caption_lines {
            line.log();
        }
        let (raw_lines, dropped_count) = self.lines.drain();
        if dropped_count > 0 {
            tracing::warn!(
                mc.detectors.dropped_lines = dropped_count,
                "the detector queue was full and dropped lines"
            );
        }
        let mut parse_failure = None;
        let mut lines: Vec<Line> = raw_lines
            .iter()
            .filter_map(|raw| match telemetry::parsed::parse_line(raw) {
                Ok(line) => Some(line),
                Err(error) => {
                    parse_failure = Some(error.to_string());
                    None
                }
            })
            .collect();
        lines.sort_by_key(|line| line.at);
        let (output, is_failure_new) = {
            let mut board = self.lock();
            let is_failure_new = parse_failure.is_some() && !board.is_parse_failure_logged;
            board.is_parse_failure_logged |= is_failure_new;
            (board.run_detectors(&lines, now), is_failure_new)
        };
        if let Some(error) = parse_failure.filter(|_| is_failure_new) {
            tracing::warn!(
                exception.message = error,
                "a detector line did not parse; later ones are not logged"
            );
        }
        for line in &output.problem_lines {
            line.log();
        }
        output.tick
    }

    pub(crate) fn view(&self, now: Moment) -> BoardView {
        let board = self.lock();
        let mut calls: Vec<CallView> = board
            .open
            .values()
            .map(|call| call.view(board.host.snapshot(&call.trace).as_ref(), now))
            .collect();
        calls.sort_by_key(|call| call.started_at);
        BoardView {
            calls,
            recent: board
                .recent
                .iter()
                .map(|ended| ended.view(board.host.snapshot(&ended.call.trace).as_ref()))
                .collect(),
            is_huddle_permission_missing: board.is_huddle_permission_missing,
        }
    }

    /// The call's status for a debug report, open or recently ended.
    pub(crate) fn describe(&self, id: &Path, now: Moment) -> Option<serde_json::Value> {
        let board = self.lock();
        let (call, ended_at) = match board.open.get(id) {
            Some(call) => (call, None),
            None => board
                .recent
                .iter()
                .find(|ended| ended.call.id == id)
                .map(|ended| (&ended.call, Some(ended.ended_at)))?,
        };
        let snapshot = board.host.snapshot(&call.trace);
        Some(call.describe(snapshot.as_ref(), ended_at, now))
    }
}

impl Board {
    fn run_detectors(&mut self, lines: &[Line], now: Moment) -> TickOutput {
        self.host.learn_suspended(lines);
        for line in lines {
            self.host.take_line(line);
        }
        self.host.advance_to(now.timestamp);
        let detected = self.host.drain_output();
        let mut output = TickOutput::default();
        for edge in &detected.edges {
            self.take_edge(edge, now, &mut output);
        }
        for notice in &detected.notices {
            self.take_notice(notice, &mut output);
        }
        for call in self.open.values_mut() {
            call.caption_state = self
                .host
                .snapshot(&call.trace)
                .map_or(CaptionState::Waiting, |snapshot| snapshot.caption_state);
        }
        for call_id in std::mem::take(&mut self.forgotten) {
            self.host.forget(&trace_of(&call_id));
            self.dumped_at
                .retain(|(dumped_call, _), _| *dumped_call != call_id);
            output.tick.forgotten_calls.push(call_id);
        }
        output
    }

    fn call_mut(&mut self, trace: &TraceKey) -> Option<&mut TrackedCall> {
        self.open
            .values_mut()
            .chain(self.recent.iter_mut().map(|ended| &mut ended.call))
            .find(|call| call.trace == *trace)
    }

    fn take_edge(&mut self, edge: &Edge, now: Moment, output: &mut TickOutput) {
        let Some(call) = self.call_mut(&edge.call) else {
            return;
        };
        let platform = call.platform;
        output.problem_lines.push(ProblemLine::Edge {
            edge: edge.clone(),
            call_id: call.id.display().to_string(),
            platform,
        });
        let raised = match edge.change {
            Change::Raised(problem) | Change::Changed { to: problem, .. } => problem,
            Change::Cleared(_) => return,
        };
        // A capture problem says nothing about the page, so it leaves the lead-up for the first
        // caption problem.
        let is_caption_problem = !raised.layer().is_capture();
        let is_first_caption_problem = is_caption_problem && !call.has_had_caption_problem;
        call.has_had_caption_problem |= is_caption_problem;
        let id = call.id.clone();
        let dump_key = (id.clone(), edge.detector);
        let is_dump_due = self
            .dumped_at
            .get(&dump_key)
            .is_none_or(|at| now.instant.saturating_duration_since(*at) >= DUMP_INTERVAL);
        if is_dump_due {
            self.dumped_at.insert(dump_key, now.instant);
            output.tick.problem_starts.push(ProblemStart {
                call_id: id,
                platform,
                problem: raised,
                is_first_caption_problem,
            });
        }
    }

    fn take_notice(&mut self, notice: &CallNotice, output: &mut TickOutput) {
        let Some(call) = self.call_mut(&notice.call) else {
            return;
        };
        output.problem_lines.push(ProblemLine::Notice {
            notice: notice.notice,
            call_id: call.id.display().to_string(),
        });
        output.tick.announcements.push(Announced {
            platform: call.platform,
            title: call.title.clone(),
            notice: notice.notice,
        });
    }
}

/// Logs the stop or resume a reopened call's log holds.
fn log_stop_from_call_log(id: &Path, is_stopped_by_user: bool) {
    if is_stopped_by_user {
        tracing::info!(
            event.name = "call.recording_stopped",
            call.id = %id.display(),
            "recording stays stopped, as the call's log says"
        );
    } else {
        tracing::info!(
            event.name = "call.recording_resumed",
            call.id = %id.display(),
            "recording runs again, as the call's log says"
        );
    }
}

/// The trace a call's lines carry, which keys it in the detectors.
fn trace_of(id: &Path) -> TraceKey {
    TraceKey(TraceId::for_call(&id.display().to_string()).to_string())
}

impl TrackedCall {
    fn new(id: &Path, platform: CallPlatform, title: Option<String>, now: Moment) -> Self {
        Self {
            id: id.to_path_buf(),
            trace: trace_of(id),
            platform,
            title,
            started_at: now.timestamp,
            caption_count: 0,
            last_caption: None,
            line_keys: HashSet::new(),
            is_stopped_by_user: false,
            is_tab_video_recorded: false,
            has_had_caption_problem: false,
            caption_state: CaptionState::Waiting,
            caption_lines: CaptionLines::default(),
        }
    }

    fn observe(&mut self, observation: Observation, now: Moment) {
        match (observation, self.platform) {
            (Observation::RecordingStopped(is_stopped), _) => {
                self.is_stopped_by_user = is_stopped;
            }
            (Observation::MeetCaption { line_key }, CallPlatform::Meet) => {
                self.line_keys.insert(line_key);
                let count = self.line_keys.len() as u64;
                self.saw_captions(count, now);
            }
            (Observation::MeetLoggedCaptions(line_keys), CallPlatform::Meet) => {
                self.line_keys.extend(line_keys);
                self.caption_count = self.caption_count.max(self.line_keys.len() as u64);
                self.caption_lines.pending_backlog_count = Some(self.caption_count);
            }
            // The watcher reads rows while stopped but logs none, so they are not captions.
            (Observation::HuddleCaptionLines(count), CallPlatform::SlackHuddle) => {
                if !self.is_stopped_by_user {
                    self.saw_captions(count, now);
                }
            }
            (Observation::TabVideoRecorded, CallPlatform::Meet) => {
                self.is_tab_video_recorded = true;
            }
            // A source reports only on its own calls.
            (
                Observation::MeetCaption { .. }
                | Observation::MeetLoggedCaptions(_)
                | Observation::TabVideoRecorded,
                CallPlatform::SlackHuddle,
            )
            | (Observation::HuddleCaptionLines(_), CallPlatform::Meet) => {}
        }
    }

    fn saw_captions(&mut self, count: u64, now: Moment) {
        self.caption_count = self.caption_count.max(count);
        self.last_caption = Some(now);
    }

    fn captions_line(&self) -> CaptionsLine {
        CaptionsLine {
            call_id: self.id.clone(),
            platform: self.platform,
            caption_count: self.caption_count,
            last_at: self.last_caption.map(|caption| caption.timestamp),
            is_backlog: false,
        }
    }

    /// Adds the call's due `call.captions` lines: a reopened log's backlog at once, then a
    /// rise in the count or a later last caption once per `CAPTION_LINE_INTERVAL`, or at once
    /// while the call waits for a caption or has a problem. A revised caption counts: a long
    /// huddle line revised for minutes is captions arriving.
    fn take_caption_lines(&mut self, now: Moment, lines: &mut Vec<CaptionsLine>) {
        let is_awaiting_captions = match self.caption_state {
            CaptionState::Waiting | CaptionState::Problem(_) => true,
            CaptionState::Recording | CaptionState::Stopped => false,
        };
        if let Some(backlog_count) = self.caption_lines.pending_backlog_count.take() {
            lines.push(CaptionsLine {
                caption_count: backlog_count,
                last_at: None,
                is_backlog: true,
                ..self.captions_line()
            });
            self.caption_lines.logged_count = self.caption_lines.logged_count.max(backlog_count);
            self.caption_lines.logged_at = Some(now.instant);
        }
        let is_interval_over = self
            .caption_lines
            .logged_at
            .is_none_or(|at| now.instant.saturating_duration_since(at) >= CAPTION_LINE_INTERVAL);
        let is_due =
            is_interval_over || is_awaiting_captions || self.caption_lines.was_awaiting_captions;
        let last_caption_at = self.last_caption.map(|caption| caption.timestamp);
        let has_new_captions = self.caption_count > self.caption_lines.logged_count
            || last_caption_at > self.caption_lines.logged_last_caption_at;
        if has_new_captions && is_due {
            lines.push(self.captions_line());
            self.caption_lines.logged_count = self.caption_count;
            self.caption_lines.logged_last_caption_at = last_caption_at;
            self.caption_lines.logged_at = Some(now.instant);
        }
        self.caption_lines.was_awaiting_captions = is_awaiting_captions;
    }

    /// The `call.captions` line the call still owes at its end.
    fn take_final_caption_line(&mut self, now: Moment) -> Option<CaptionsLine> {
        let mut lines = Vec::new();
        self.caption_lines.logged_at = None;
        self.take_caption_lines(now, &mut lines);
        // A backlog line, if any, comes first; the count line covers it.
        lines.pop()
    }

    fn state(&self, snapshot: Option<&CallSnapshot>) -> CallState {
        if self.is_stopped_by_user {
            return CallState::Stopped;
        }
        // Until the detectors read the call's first line, it waits.
        match snapshot.map_or(CaptionState::Waiting, |snapshot| snapshot.caption_state) {
            CaptionState::Waiting => CallState::Waiting,
            CaptionState::Recording => CallState::Recording,
            CaptionState::Problem(problem) => CallState::Problem(problem),
            CaptionState::Stopped => CallState::Stopped,
        }
    }

    fn view(&self, snapshot: Option<&CallSnapshot>, now: Moment) -> CallView {
        let caption_source = snapshot
            .and_then(|snapshot| snapshot.caption_source)
            .map(|source| match source {
                CaptionSource::Channel => HealthSource::Channel,
                CaptionSource::Dom => HealthSource::Dom,
                CaptionSource::None => HealthSource::None,
            });
        let huddle_health = snapshot
            .and_then(|snapshot| snapshot.huddle_health)
            .map(|health| match health {
                detectors::HuddleHealth::Healthy => HuddleHealth::Healthy,
                detectors::HuddleHealth::Degraded => HuddleHealth::Degraded,
                detectors::HuddleHealth::CaptionsOff => HuddleHealth::CaptionsOff,
                detectors::HuddleHealth::Broken => HuddleHealth::Broken,
                detectors::HuddleHealth::Untested => HuddleHealth::Untested,
            });
        CallView {
            id: self.id.clone(),
            platform: self.platform,
            title: self.title.clone(),
            state: self.state(snapshot),
            started_at: self.started_at,
            recording_since: snapshot.and_then(|snapshot| snapshot.recording_since),
            caption_count: self.caption_count,
            last_caption_at: self.last_caption.map(|caption| caption.timestamp),
            last_caption_age: self
                .last_caption
                .map(|caption| now.instant.saturating_duration_since(caption.instant)),
            caption_source,
            huddle_health,
            problems: snapshot
                .map_or_else(Vec::new, |snapshot| caption_problems(&snapshot.history)),
            capture_problems: snapshot
                .map_or_else(Vec::new, |snapshot| snapshot.capture_problems.clone()),
        }
    }

    fn describe(
        &self,
        snapshot: Option<&CallSnapshot>,
        ended_at: Option<Timestamp>,
        now: Moment,
    ) -> serde_json::Value {
        let state = self.state(snapshot);
        let state_code = match (ended_at, state) {
            (Some(_), _) => "ended",
            (None, CallState::Waiting) => "waiting",
            (None, CallState::Recording) => "recording",
            (None, CallState::Problem(_)) => "problem",
            (None, CallState::Stopped) => "stopped",
        };
        let reason = match state {
            CallState::Problem(problem) if ended_at.is_none() => Some(problem.code()),
            CallState::Problem(_)
            | CallState::Waiting
            | CallState::Recording
            | CallState::Stopped => None,
        };
        let history = snapshot.map_or(&[][..], |snapshot| snapshot.history.as_slice());
        let failing_layer =
            detectors::verdict(history, ended_at.unwrap_or(now.timestamp)).map(|verdict| {
                serde_json::json!({
                    "layer": verdict.layer.code(),
                    "problem": verdict.problem.code(),
                    "raised_at": verdict.raised_at,
                    "duration_s": verdict.duration.as_secs(),
                    "is_noticed": verdict.is_noticed,
                })
            });
        let capture_problems: Vec<&str> = snapshot.map_or_else(Vec::new, |snapshot| {
            snapshot
                .capture_problems
                .iter()
                .map(|open| open.problem.code())
                .collect()
        });
        serde_json::json!({
            "call_id": self.id,
            "platform": self.platform.as_str(),
            "trace_id": self.trace.0,
            "state": state_code,
            "reason": reason,
            "started_at": self.started_at,
            "ended_at": ended_at,
            "recording_since": snapshot.and_then(|snapshot| snapshot.recording_since),
            "caption_count": self.caption_count,
            "last_caption_at": self.last_caption.map(|caption| caption.timestamp),
            "problems": caption_problems(history)
                .iter()
                .map(|record| serde_json::json!({ "reason": record.problem.code(), "at": record.at }))
                .collect::<Vec<_>>(),
            "capture_problems": capture_problems,
            "problem_history": history.iter().map(SpanRecord::of).collect::<Vec<_>>(),
            "failing_layer": failing_layer,
            "described_at": now.timestamp,
        })
    }
}

/// The last caption problems, oldest first, as mc-meet shows them.
fn caption_problems(history: &[ProblemSpan]) -> Vec<ProblemRecord> {
    let records: Vec<ProblemRecord> = history
        .iter()
        .filter(|span| !span.problem.layer().is_capture())
        .map(|span| ProblemRecord {
            problem: span.problem,
            at: span.raised_at,
        })
        .collect();
    let skipped = records.len().saturating_sub(KEPT_PROBLEMS);
    records.into_iter().skip(skipped).collect()
}

/// One problem span as a debug report shows it.
#[derive(Serialize)]
struct SpanRecord {
    code: &'static str,
    layer: &'static str,
    detector: &'static str,
    raised_at: Timestamp,
    noticed_at: Option<Timestamp>,
    cleared_at: Option<Timestamp>,
}

impl SpanRecord {
    fn of(span: &ProblemSpan) -> Self {
        Self {
            code: span.problem.code(),
            layer: span.problem.layer().code(),
            detector: span.detector.code(),
            raised_at: span.raised_at,
            noticed_at: span.noticed_at,
            cleared_at: span.cleared_at,
        }
    }
}

impl EndedCall {
    fn view(&self, snapshot: Option<&CallSnapshot>) -> EndedCallView {
        let call = &self.call;
        let has_problems = snapshot.is_some_and(|snapshot| !snapshot.history.is_empty());
        let outcome = match (call.caption_count, has_problems) {
            (0, _) => CallOutcome::NoTranscript,
            (_, false) => CallOutcome::Recorded,
            (_, true) => CallOutcome::RecordedWithProblems,
        };
        EndedCallView {
            id: call.id.clone(),
            platform: call.platform,
            title: call.title.clone(),
            started_at: call.started_at,
            ended_at: self.ended_at,
            caption_count: call.caption_count,
            outcome,
            is_tab_video_recorded: call.is_tab_video_recorded,
        }
    }
}

impl CallView {
    /// One line for the tray menu and tooltip, times in `time_zone`.
    pub(crate) fn status_line(&self, time_zone: &TimeZone) -> String {
        let name = call_name(self.platform, self.title.as_deref());
        let captions = match (self.caption_count, self.last_caption_age) {
            (0, _) | (_, None) => "no captions yet".to_owned(),
            (count, Some(age)) => format!(
                "{count} caption{}, last {} ago",
                plural(count),
                short_age(age)
            ),
        };
        let source_note = match (self.caption_source, self.huddle_health) {
            (Some(HealthSource::Dom), _) => ", from the page",
            (_, Some(HuddleHealth::Degraded)) => ", partly read",
            (_, Some(HuddleHealth::Untested)) => ", untested Slack version",
            (
                Some(HealthSource::Channel | HealthSource::None) | None,
                Some(HuddleHealth::Healthy | HuddleHealth::CaptionsOff | HuddleHealth::Broken)
                | None,
            ) => "",
        };
        match self.state {
            CallState::Waiting => format!(
                "{name}: waiting for captions since {}",
                clock(self.started_at, time_zone)
            ),
            CallState::Recording => format!(
                "{name}: recording since {}, {captions}{source_note}",
                clock(self.recording_since.unwrap_or(self.started_at), time_zone)
            ),
            CallState::Problem(problem) => {
                format!("{name}: not recording, {}; {captions}", problem.describe())
            }
            CallState::Stopped => format!("{name}: recording stopped by you; {captions}"),
        }
    }

    /// One tray row per capture problem, such as "Audio problem: your mic is lost, since 10:35
    /// IST".
    pub(crate) fn capture_problem_lines(&self, time_zone: &TimeZone) -> Vec<String> {
        self.capture_problems
            .iter()
            .map(|open| {
                format!(
                    "{}: {}, since {}",
                    capture_heading(open.problem),
                    open.problem.describe(),
                    clock(open.since, time_zone)
                )
            })
            .collect()
    }
}

fn capture_medium(problem: Problem) -> &'static str {
    if problem.layer().is_audio() {
        "Audio"
    } else {
        "Video"
    }
}

fn capture_heading(problem: Problem) -> String {
    format!("{} problem", capture_medium(problem))
}

impl EndedCallView {
    /// One line for the recent calls list, times in `time_zone`.
    pub(crate) fn summary_line(&self, time_zone: &TimeZone) -> String {
        let name = call_name(self.platform, self.title.as_deref());
        let start = self
            .started_at
            .to_zoned(time_zone.clone())
            .strftime("%d %b %H:%M");
        let end = clock(self.ended_at, time_zone);
        let minutes = self.ended_at.duration_since(self.started_at).as_secs() / 60;
        let outcome = match self.outcome {
            CallOutcome::Recorded => "recorded",
            CallOutcome::RecordedWithProblems => "recorded with problems",
            CallOutcome::NoTranscript => "no transcript",
        };
        // Only the Mac records video, and only of Meet tabs.
        let is_video_missing = match self.platform {
            CallPlatform::Meet => cfg!(target_os = "macos") && !self.is_tab_video_recorded,
            CallPlatform::SlackHuddle => false,
        };
        let video_note = if is_video_missing { ", no video" } else { "" };
        format!(
            "{start} to {end}  {name}: {} caption{} in {minutes} min, {outcome}{video_note}",
            self.caption_count,
            plural(self.caption_count)
        )
    }
}

/// "Meet <title>" or "Slack huddle <channel>".
pub(crate) fn call_name(platform: CallPlatform, title: Option<&str>) -> String {
    let title = crate::tray::shorten(title.unwrap_or("call"), 40);
    match platform {
        CallPlatform::Meet => format!("Meet {title}"),
        CallPlatform::SlackHuddle => format!("Slack huddle {title}"),
    }
}

/// A clock time such as `21:02 IST`.
pub(crate) fn clock(at: Timestamp, time_zone: &TimeZone) -> String {
    at.to_zoned(time_zone.clone())
        .strftime("%H:%M %Z")
        .to_string()
}

fn short_age(age: Duration) -> String {
    let seconds = age.as_secs();
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3600),
    }
}

fn plural(count: u64) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// The time zone times are shown in: the settings' one, else the device's.
pub(crate) fn display_time_zone(state: &AppState) -> TimeZone {
    state
        .pipeline
        .as_ref()
        .map_or_else(TimeZone::system, |pipeline| {
            pipeline.settings().time_zone.clone()
        })
}

/// Ticks the board once a second: notifications, debug capture at a problem, the tray, and the
/// status mc-meet shows.
pub(crate) fn start(app: &AppHandle) {
    tauri::async_runtime::spawn(watch_calls(app.clone()));
}

async fn watch_calls(app: AppHandle) {
    let mut interval = tokio::time::interval(TICK_INTERVAL);
    let mut had_calls_at_last_tick = false;
    let mut last_tick: Option<Moment> = None;
    loop {
        interval.tick().await;
        let now = Moment::now();
        if let Some(last) = last_tick {
            log_host_suspended(last, now);
        }
        last_tick = Some(now);
        let state = app.state::<AppState>();
        let board = state.calls.status();
        let tick = board.tick(now);
        for start in &tick.problem_starts {
            report_problem_start(&app, start);
        }
        for announced in &tick.announcements {
            announce(&app, announced);
        }
        for call_id in &tick.forgotten_calls {
            state.call_debug.forget_call(call_id);
        }
        let view = board.view(Moment::now());
        state
            .browser_calls
            .publish_call_status(&view, &display_time_zone(&state));
        let has_open_calls = !view.calls.is_empty();
        // The status line ages every second while a call is open; one last update clears it.
        if has_open_calls || had_calls_at_last_tick {
            crate::tray::show_call_status(&app);
        }
        had_calls_at_last_tick = has_open_calls;
    }
}

/// Logs `host.suspended` when the wall clock moved over `SUSPEND_LIMIT` more than the
/// monotonic clock since the last tick. On macOS `Instant` stops while the host sleeps.
fn log_host_suspended(last: Moment, now: Moment) {
    let wall_elapsed = now.timestamp.duration_since(last.timestamp);
    let Ok(wall_elapsed) = Duration::try_from(wall_elapsed) else {
        return;
    };
    let suspended =
        wall_elapsed.saturating_sub(now.instant.saturating_duration_since(last.instant));
    if suspended > SUSPEND_LIMIT {
        tracing::info!(
            event.name = "host.suspended",
            mc.host.suspended_ms = u64::try_from(suspended.as_millis()).unwrap_or(u64::MAX),
            "the host was suspended"
        );
    }
}

fn report_problem_start(app: &AppHandle, start: &ProblemStart) {
    let state = app.state::<AppState>();
    // Every platform and layer: the recorders hold what the call's own log does not.
    crate::call_debug::dump_recorder_in_background(
        app,
        start.call_id.clone(),
        telemetry::DumpReason::CallProblem,
    );
    state.call_capture.request_recorder_dumps(&start.call_id);
    // The lead-up and the page snapshot show what the Meet page did about captions. The huddle
    // log already holds every health change the watcher saw.
    let is_meet_caption_problem = match start.platform {
        CallPlatform::Meet => !start.problem.layer().is_capture(),
        CallPlatform::SlackHuddle => false,
    };
    if is_meet_caption_problem {
        if start.is_first_caption_problem {
            state.call_debug.write_lead_up(&start.call_id);
        }
        if state.call_debug.take_snapshot_turn(&start.call_id) {
            state.browser_calls.request_page_snapshot(&start.call_id);
        }
    }
}

fn announce(app: &AppHandle, announced: &Announced) {
    let name = call_name(announced.platform, announced.title.as_deref());
    let source = match announced.platform {
        CallPlatform::Meet => "the Meet call",
        CallPlatform::SlackHuddle => "the Slack huddle",
    };
    let (heading, body) = match announced.notice {
        Notice::Recording => (
            format!("Recording {name}"),
            format!("Captions arrive from {source}."),
        ),
        Notice::NotRecording(problem) => (
            format!("Not recording {name}"),
            capitalise(problem.describe()),
        ),
        Notice::RecordingAgain => (
            format!("Recording again {name}"),
            format!("Captions arrive from {source} again."),
        ),
        Notice::NoTranscript => (
            format!("No transcript captured for {name}"),
            "The call ended with no caption. \"Save debug report\" in the tray keeps what \
             happened."
                .to_owned(),
        ),
        Notice::CaptureTrouble(problem) => (
            format!("{} in {name}", capture_heading(problem)),
            format!(
                "{}. Captions still record. \"Save debug report\" in the tray keeps what \
                 happened.",
                capitalise(problem.describe())
            ),
        ),
        Notice::CaptureBack(problem) => {
            let medium = capture_medium(problem);
            (
                format!("{medium} back in {name}"),
                format!("{medium} records again."),
            )
        }
    };
    crate::notify(app, &heading, &body);
}

fn capitalise(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}
