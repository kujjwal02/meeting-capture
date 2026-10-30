//! What each call is doing now: waiting for its first caption, recording, or not recording and
//! why. Meet calls and Slack huddles both feed it. The board decides each call's state from
//! timestamped observations and says what to announce; the tray, the notifications and mc-meet
//! only show it.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use huddle_captions::HuddleHealth;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;
use sources::browser_call::HealthSource;
use tauri::{AppHandle, Manager as _};

use crate::AppState;
use crate::calls::CallPlatform;

const TICK_INTERVAL: Duration = Duration::from_secs(1);
/// With a cause in view, such as captions off, no caption for this long is a problem. The
/// extension needs a few seconds to turn captions on at call start.
const CAPTION_STALL: Duration = Duration::from_secs(20);
/// No caption for this long while captions are on is a problem by itself. People can be silent
/// for a minute, at the start of a call or during a screen share.
const SILENCE_LIMIT: Duration = Duration::from_mins(2);
/// A problem that lasts this long gets a notification. Shorter ones are reconnects and pauses.
const PROBLEM_NOTICE_AFTER: Duration = Duration::from_secs(30);
const KEPT_PROBLEMS: usize = 3;
const RECENT_CALLS: usize = 5;

/// Why a call is not recording. One closed list for Meet and Slack huddles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProblemReason {
    /// Meet's or Slack's captions are off, so no caption arrives.
    CaptionsOff,
    /// Meet refuses captions: its captions button is disabled or says they are unavailable.
    MeetCaptionsUnavailable,
    /// Frames arrive on a caption channel, and none of them decode.
    CaptionParseFailures,
    /// Captions are on and nothing arrived for `SILENCE_LIMIT`.
    CaptionsSilent,
    /// Nothing arrived for `SILENCE_LIMIT`, and the source cannot tell whether captions are on.
    NoCaptions,
    /// No mc-meet port holds the call: the browser quit or crashed, or the extension stopped.
    ExtensionDisconnected,
    /// Every Meet tab of the call closed, reloaded or crashed, and none came back yet. mc-meet
    /// ends the call if none comes back within 5 minutes.
    MeetPageAway,
    /// No layout profile can read the huddle window.
    HuddleCaptionsUnreadable,
}

impl ProblemReason {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::CaptionsOff => "captions_off",
            Self::MeetCaptionsUnavailable => "meet_captions_unavailable",
            Self::CaptionParseFailures => "caption_parse_failures",
            Self::CaptionsSilent => "captions_silent",
            Self::NoCaptions => "no_captions",
            Self::ExtensionDisconnected => "extension_disconnected",
            Self::MeetPageAway => "meet_page_away",
            Self::HuddleCaptionsUnreadable => "huddle_captions_unreadable",
        }
    }

    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::CaptionsOff => "captions are off",
            Self::MeetCaptionsUnavailable => "Meet captions unavailable (Google)",
            Self::CaptionParseFailures => "captions arrive but cannot be read",
            Self::CaptionsSilent => "no caption for 2 min while captions are on",
            Self::NoCaptions => "no caption for 2 min",
            Self::ExtensionDisconnected => "mc-meet disconnected",
            Self::MeetPageAway => "the Meet tab closed or reloaded",
            Self::HuddleCaptionsUnreadable => "the huddle window cannot be read",
        }
    }

    // A tab that went away usually means the call ended, and mc-meet ends it in 5 minutes if no
    // tab comes back. A disconnect with no mc-meet port left is a browser that quit; with one
    // left, the extension lost the call while the browser runs.
    fn deserves_notice(self, is_extension_connected: bool) -> bool {
        match self {
            Self::MeetPageAway => false,
            Self::ExtensionDisconnected => is_extension_connected,
            Self::CaptionsOff
            | Self::MeetCaptionsUnavailable
            | Self::CaptionParseFailures
            | Self::CaptionsSilent
            | Self::NoCaptions
            | Self::HuddleCaptionsUnreadable => true,
        }
    }
}

/// An open call's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallState {
    /// The call is open and no caption has arrived yet.
    Waiting,
    /// A caption arrived, and no problem is in view.
    Recording,
    Problem(ProblemReason),
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

/// What Meet's captions button says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MeetCaptions {
    On,
    Off,
    /// Meet refuses captions: the button is disabled or says they are unavailable.
    Unavailable,
}

/// One fact a source reports about an open call.
#[derive(Debug, Clone)]
pub(crate) enum Observation {
    /// A Meet caption event. Every version of one utterance has the same `line_key`.
    MeetCaption {
        line_key: String,
    },
    MeetHealth {
        source: HealthSource,
        captions: MeetCaptions,
        is_caption_frames_failing: bool,
    },
    /// Whether any mc-meet port holds the call.
    MeetPortHeld(bool),
    /// Whether every tab of the call went away and mc-meet waits for one to come back.
    MeetPagesAway(bool),
    /// The caption lines a reopened log already holds, by line key. They count, but say nothing
    /// about whether captions arrive now.
    MeetLoggedCaptions(Vec<String>),
    /// Caption lines arrived or changed, and the huddle log holds this many.
    HuddleCaptionLines(u64),
    HuddleHealth(HuddleHealth),
    /// The user stopped recording the call (`true`) or resumed it (`false`).
    RecordingStopped(bool),
}

/// What the user hears about a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Announcement {
    /// The first caption arrived.
    Recording,
    /// A problem lasted `PROBLEM_NOTICE_AFTER`. Once per reason in one outage.
    NotRecording(ProblemReason),
    /// A caption arrived after an announced problem. It ends the outage.
    RecordingAgain,
    /// The call ended with no caption.
    NoTranscript,
}

#[derive(Debug, Clone)]
pub(crate) struct Announced {
    pub platform: CallPlatform,
    pub title: Option<String>,
    pub announcement: Announcement,
}

/// A call whose problem started at this tick.
#[derive(Debug, Clone)]
pub(crate) struct ProblemStart {
    pub call_id: PathBuf,
    pub platform: CallPlatform,
    /// The call's first problem in this run.
    pub is_first: bool,
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

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct ProblemRecord {
    pub reason: ProblemReason,
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
    /// The last problems, oldest first.
    pub problems: Vec<ProblemRecord>,
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
        let has_state =
            |is_wanted: fn(CallState) -> bool| self.calls.iter().any(|call| is_wanted(call.state));
        if self.is_huddle_permission_missing
            || has_state(|state| matches!(state, CallState::Problem(_)))
        {
            TrayLook::Problem
        } else if has_state(|state| matches!(state, CallState::Recording)) {
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

/// The open calls, the last ended ones, and what was announced.
pub(crate) struct CallBoard {
    board: Mutex<Board>,
}

#[derive(Default)]
struct Board {
    open: HashMap<PathBuf, TrackedCall>,
    /// Newest first.
    recent: VecDeque<EndedCall>,
    is_huddle_permission_missing: bool,
    /// Whether any mc-meet port is connected.
    is_extension_connected: bool,
    /// Announcements made outside a tick, such as at a call end.
    pending: Vec<Announced>,
    /// Calls that left `recent`, for the next tick.
    forgotten: Vec<PathBuf>,
}

struct TrackedCall {
    id: PathBuf,
    platform: CallPlatform,
    title: Option<String>,
    started_at: Timestamp,
    /// Since when the call waits for a caption: its open, or its resume. Only a caption after
    /// it shows the call as recording.
    waiting_since: Instant,
    caption_count: u64,
    last_caption: Option<Moment>,
    recording_since: Option<Timestamp>,
    signals: Signals,
    problem: Option<ActiveProblem>,
    problems: VecDeque<ProblemRecord>,
    notices: CallNotices,
    /// "Stop recording this call" was chosen and "Resume recording" was not. A rejoin
    /// (`call.resumed`) keeps it.
    is_stopped_by_user: bool,
}

/// What the call announced, so each notification goes out once, even when a state flips back
/// and forth.
#[derive(Default)]
struct CallNotices {
    is_recording_announced: bool,
    /// The reasons announced in the current outage. A "Recording again" ends the outage.
    outage_reasons: HashSet<ProblemReason>,
    /// When the last "Not recording" was announced. A caption after it is a recovery.
    recovery_owed_since: Option<Instant>,
    is_no_transcript_announced: bool,
}

struct EndedCall {
    call: TrackedCall,
    ended_at: Timestamp,
}

struct ActiveProblem {
    reason: ProblemReason,
    /// When the call stopped recording, kept when the reason changes.
    since: Instant,
    is_announced: bool,
}

enum Signals {
    Meet(MeetSignals),
    Huddle(HuddleSignals),
}

struct MeetSignals {
    is_port_held: bool,
    is_pages_away: bool,
    captions: MeetCaptions,
    is_caption_frames_failing: bool,
    caption_source: Option<HealthSource>,
    /// Device and utterance of every caption line, so revisions count once.
    line_keys: HashSet<String>,
}

struct HuddleSignals {
    health: Option<HuddleHealth>,
}

impl CallBoard {
    pub(crate) fn new() -> Self {
        Self {
            board: Mutex::new(Board::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Board> {
        self.board.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A call opened, or one that ended in this run opened again and keeps its counts.
    /// `is_stopped_by_user` comes from the call's log, so a stop survives a restart.
    pub(crate) fn open(
        &self,
        id: &Path,
        platform: CallPlatform,
        title: Option<String>,
        is_stopped_by_user: bool,
        now: Moment,
    ) {
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
                call.resume(title, now);
                call
            }
            None => TrackedCall::new(id, platform, title, now),
        };
        call.is_stopped_by_user = is_stopped_by_user;
        board.open.insert(id.to_path_buf(), call);
    }

    /// A call ended. One with no caption gets a "No transcript captured" notification.
    pub(crate) fn end(&self, id: &Path, now: Moment) {
        let mut board = self.lock();
        let Some(mut call) = board.open.remove(id) else {
            return;
        };
        if call.caption_count == 0 && !call.notices.is_no_transcript_announced {
            call.notices.is_no_transcript_announced = true;
            board
                .pending
                .push(call.announced(Announcement::NoTranscript));
        }
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

    pub(crate) fn set_extension_connected(&self, is_connected: bool) {
        self.lock().is_extension_connected = is_connected;
    }

    /// Works out every open call's state at `now` and returns what to announce.
    pub(crate) fn tick(&self, now: Moment) -> Tick {
        let mut board = self.lock();
        let board = &mut *board;
        let mut problem_starts = Vec::new();
        for call in board.open.values_mut() {
            problem_starts.extend(call.settle(
                now,
                board.is_extension_connected,
                &mut board.pending,
            ));
        }
        Tick {
            announcements: std::mem::take(&mut board.pending),
            problem_starts,
            forgotten_calls: std::mem::take(&mut board.forgotten),
        }
    }

    pub(crate) fn view(&self, now: Moment) -> BoardView {
        let board = self.lock();
        let mut calls: Vec<CallView> = board.open.values().map(|call| call.view(now)).collect();
        calls.sort_by_key(|call| call.started_at);
        BoardView {
            calls,
            recent: board.recent.iter().map(EndedCall::view).collect(),
            is_huddle_permission_missing: board.is_huddle_permission_missing,
        }
    }

    /// The call's status for a debug report, open or recently ended.
    pub(crate) fn describe(&self, id: &Path, now: Moment) -> Option<serde_json::Value> {
        let board = self.lock();
        if let Some(call) = board.open.get(id) {
            return Some(call.describe(None, now));
        }
        board
            .recent
            .iter()
            .find(|ended| ended.call.id == id)
            .map(|ended| ended.call.describe(Some(ended.ended_at), now))
    }
}

impl TrackedCall {
    fn new(id: &Path, platform: CallPlatform, title: Option<String>, now: Moment) -> Self {
        let signals = match platform {
            CallPlatform::Meet => Signals::Meet(MeetSignals {
                is_port_held: true,
                is_pages_away: false,
                captions: MeetCaptions::Off,
                is_caption_frames_failing: false,
                caption_source: None,
                line_keys: HashSet::new(),
            }),
            CallPlatform::SlackHuddle => Signals::Huddle(HuddleSignals { health: None }),
        };
        Self {
            id: id.to_path_buf(),
            platform,
            title,
            started_at: now.timestamp,
            waiting_since: now.instant,
            caption_count: 0,
            last_caption: None,
            recording_since: None,
            signals,
            problem: None,
            problems: VecDeque::new(),
            notices: CallNotices::default(),
            is_stopped_by_user: false,
        }
    }

    /// The call opened again after it ended in this run: a rejoin, or late events after an
    /// idle end. Its counts and problems stay; what the old page said about captions does not.
    fn resume(&mut self, title: Option<String>, now: Moment) {
        self.waiting_since = now.instant;
        self.recording_since = None;
        self.problem = None;
        self.title = title.or(self.title.take());
        self.notices.outage_reasons.clear();
        self.notices.recovery_owed_since = None;
        if let Signals::Meet(meet) = &mut self.signals {
            meet.is_port_held = true;
            meet.is_pages_away = false;
            meet.captions = MeetCaptions::Off;
            meet.is_caption_frames_failing = false;
            meet.caption_source = None;
        }
    }

    /// Whether a caption arrived since the call opened or resumed.
    fn has_caption_since_wait(&self) -> bool {
        self.last_caption
            .is_some_and(|caption| caption.instant >= self.waiting_since)
    }

    fn announced(&self, announcement: Announcement) -> Announced {
        Announced {
            platform: self.platform,
            title: self.title.clone(),
            announcement,
        }
    }

    fn observe(&mut self, observation: Observation, now: Moment) {
        match (observation, &mut self.signals) {
            (Observation::RecordingStopped(is_stopped), _) => {
                // A resume waits for a fresh caption, as a rejoin does.
                if self.is_stopped_by_user && !is_stopped {
                    self.waiting_since = now.instant;
                    self.recording_since = None;
                }
                self.is_stopped_by_user = is_stopped;
                self.problem = None;
            }
            (Observation::MeetCaption { line_key }, Signals::Meet(meet)) => {
                meet.line_keys.insert(line_key);
                let count = meet.line_keys.len() as u64;
                self.saw_captions(count, now);
            }
            (
                Observation::MeetHealth {
                    source,
                    captions,
                    is_caption_frames_failing,
                },
                Signals::Meet(meet),
            ) => {
                meet.caption_source = Some(source);
                meet.captions = captions;
                meet.is_caption_frames_failing = is_caption_frames_failing;
            }
            (Observation::MeetPortHeld(is_held), Signals::Meet(meet)) => {
                meet.is_port_held = is_held;
            }
            (Observation::MeetPagesAway(is_away), Signals::Meet(meet)) => {
                meet.is_pages_away = is_away;
            }
            (Observation::MeetLoggedCaptions(line_keys), Signals::Meet(meet)) => {
                meet.line_keys.extend(line_keys);
                self.caption_count = self.caption_count.max(meet.line_keys.len() as u64);
            }
            // The watcher reads rows while stopped but logs none, so they are not captions.
            (Observation::HuddleCaptionLines(count), Signals::Huddle(_)) => {
                if !self.is_stopped_by_user {
                    self.saw_captions(count, now);
                }
            }
            (Observation::HuddleHealth(health), Signals::Huddle(huddle)) => {
                huddle.health = Some(health);
            }
            // A source reports only on its own calls.
            (
                Observation::MeetCaption { .. }
                | Observation::MeetHealth { .. }
                | Observation::MeetPortHeld(_)
                | Observation::MeetPagesAway(_)
                | Observation::MeetLoggedCaptions(_),
                Signals::Huddle(_),
            )
            | (
                Observation::HuddleCaptionLines(_) | Observation::HuddleHealth(_),
                Signals::Meet(_),
            ) => {}
        }
    }

    fn saw_captions(&mut self, count: u64, now: Moment) {
        if self.recording_since.is_none() || self.problem.is_some() {
            self.recording_since = Some(now.timestamp);
        }
        self.caption_count = self.caption_count.max(count);
        self.last_caption = Some(now);
    }

    // How long the call has gone without a caption.
    fn quiet_for(&self, now: Moment) -> Duration {
        let since = self.last_caption.map_or(self.waiting_since, |caption| {
            caption.instant.max(self.waiting_since)
        });
        now.instant.saturating_duration_since(since)
    }

    fn current_problem(&self, now: Moment) -> Option<ProblemReason> {
        if self.is_stopped_by_user {
            return None;
        }
        let quiet_for = self.quiet_for(now);
        match &self.signals {
            Signals::Meet(meet) => {
                if !meet.is_port_held {
                    return Some(ProblemReason::ExtensionDisconnected);
                }
                if quiet_for < CAPTION_STALL {
                    return None;
                }
                if meet.is_pages_away {
                    return Some(ProblemReason::MeetPageAway);
                }
                // mc-meet hides captions once a caption arrived on its own channels, which keep
                // streaming. Only then is "off" no cause. mc-meet reports the channel source
                // only after a caption on the channels the page has now.
                let is_off_a_cause = meet.caption_source != Some(HealthSource::Channel);
                // The DOM reader does not need the frames.
                let is_frames_failing = meet.is_caption_frames_failing
                    && meet.caption_source != Some(HealthSource::Dom);
                match meet.captions {
                    MeetCaptions::Unavailable => Some(ProblemReason::MeetCaptionsUnavailable),
                    MeetCaptions::On | MeetCaptions::Off if is_frames_failing => {
                        Some(ProblemReason::CaptionParseFailures)
                    }
                    MeetCaptions::Off if is_off_a_cause => Some(ProblemReason::CaptionsOff),
                    MeetCaptions::On | MeetCaptions::Off => {
                        (quiet_for >= SILENCE_LIMIT).then_some(ProblemReason::CaptionsSilent)
                    }
                }
            }
            Signals::Huddle(huddle) => match huddle.health {
                Some(HuddleHealth::CaptionsOff) => Some(ProblemReason::CaptionsOff),
                Some(HuddleHealth::Broken) => Some(ProblemReason::HuddleCaptionsUnreadable),
                Some(HuddleHealth::Healthy | HuddleHealth::Degraded) => {
                    (quiet_for >= SILENCE_LIMIT).then_some(ProblemReason::CaptionsSilent)
                }
                // No verdict on the window yet, or a Slack version no profile was tested on.
                Some(HuddleHealth::Untested) | None => {
                    (quiet_for >= SILENCE_LIMIT).then_some(ProblemReason::NoCaptions)
                }
            },
        }
    }

    fn state(&self) -> CallState {
        match &self.problem {
            _ if self.is_stopped_by_user => CallState::Stopped,
            Some(problem) => CallState::Problem(problem.reason),
            None if self.has_caption_since_wait() => CallState::Recording,
            None => CallState::Waiting,
        }
    }

    /// Moves the call to its state at `now`. Returns a problem that started, and adds what to
    /// announce to `announcements`.
    fn settle(
        &mut self,
        now: Moment,
        is_extension_connected: bool,
        announcements: &mut Vec<Announced>,
    ) -> Option<ProblemStart> {
        let current = self.current_problem(now);
        let mut start = None;
        let mut wanted = None;
        match (&mut self.problem, current) {
            (None, None) => {}
            (Some(problem), Some(reason)) if problem.reason == reason => {
                let has_lasted =
                    now.instant.saturating_duration_since(problem.since) >= PROBLEM_NOTICE_AFTER;
                if has_lasted
                    && !problem.is_announced
                    && reason.deserves_notice(is_extension_connected)
                {
                    problem.is_announced = true;
                    self.notices.recovery_owed_since = Some(now.instant);
                    if self.notices.outage_reasons.insert(reason) {
                        wanted = Some(Announcement::NotRecording(reason));
                    }
                }
            }
            (problem, Some(reason)) => {
                let is_first = self.problems.is_empty();
                let since = problem
                    .as_ref()
                    .map_or(now.instant, |problem| problem.since);
                *problem = Some(ActiveProblem {
                    reason,
                    since,
                    is_announced: false,
                });
                self.problems.push_back(ProblemRecord {
                    reason,
                    at: now.timestamp,
                });
                if self.problems.len() > KEPT_PROBLEMS {
                    self.problems.pop_front();
                }
                tracing::warn!(
                    event.name = "call.problem",
                    call.id = %self.id.display(),
                    call.platform = self.platform.as_str(),
                    error.type = reason.code(),
                    "call not recording: {}",
                    reason.describe()
                );
                start = Some(ProblemStart {
                    call_id: self.id.clone(),
                    platform: self.platform,
                    is_first,
                });
            }
            (problem @ Some(_), None) => {
                *problem = None;
                tracing::info!(
                    event.name = "call.recovered",
                    call.id = %self.id.display(),
                    call.platform = self.platform.as_str(),
                    "call problem cleared"
                );
            }
        }
        if self.state() == CallState::Recording {
            // The first "Recording" also tells of a recovery from a problem before it.
            let is_recovered = self
                .notices
                .recovery_owed_since
                .zip(self.last_caption)
                .is_some_and(|(owed_since, caption)| caption.instant > owed_since);
            if !self.notices.is_recording_announced {
                self.notices.is_recording_announced = true;
                self.end_outage();
                wanted = Some(Announcement::Recording);
            } else if is_recovered {
                self.end_outage();
                wanted = Some(Announcement::RecordingAgain);
            }
        }
        if let Some(announcement) = wanted {
            announcements.push(self.announced(announcement));
        }
        start
    }

    // A later outage, even with the same reason, is announced again. `PROBLEM_NOTICE_AFTER`
    // keeps short flaps quiet.
    fn end_outage(&mut self) {
        self.notices.outage_reasons.clear();
        self.notices.recovery_owed_since = None;
    }

    fn view(&self, now: Moment) -> CallView {
        let (caption_source, huddle_health) = match &self.signals {
            Signals::Meet(meet) => (meet.caption_source, None),
            Signals::Huddle(huddle) => (None, huddle.health),
        };
        CallView {
            id: self.id.clone(),
            platform: self.platform,
            title: self.title.clone(),
            state: self.state(),
            started_at: self.started_at,
            recording_since: self.recording_since,
            caption_count: self.caption_count,
            last_caption_at: self.last_caption.map(|caption| caption.timestamp),
            last_caption_age: self
                .last_caption
                .map(|caption| now.instant.saturating_duration_since(caption.instant)),
            caption_source,
            huddle_health,
            problems: self.problems.iter().copied().collect(),
        }
    }

    fn describe(&self, ended_at: Option<Timestamp>, now: Moment) -> serde_json::Value {
        let state = match (ended_at, self.state()) {
            (Some(_), _) => "ended",
            (None, CallState::Waiting) => "waiting",
            (None, CallState::Recording) => "recording",
            (None, CallState::Problem(_)) => "problem",
            (None, CallState::Stopped) => "stopped",
        };
        let reason = match self.state() {
            CallState::Problem(reason) if ended_at.is_none() => Some(reason.code()),
            CallState::Problem(_)
            | CallState::Waiting
            | CallState::Recording
            | CallState::Stopped => None,
        };
        serde_json::json!({
            "call_id": self.id,
            "platform": self.platform.as_str(),
            "state": state,
            "reason": reason,
            "started_at": self.started_at,
            "ended_at": ended_at,
            "recording_since": self.recording_since,
            "caption_count": self.caption_count,
            "last_caption_at": self.last_caption.map(|caption| caption.timestamp),
            "problems": self.problems,
            "described_at": now.timestamp,
        })
    }
}

impl EndedCall {
    fn view(&self) -> EndedCallView {
        let call = &self.call;
        let outcome = match (call.caption_count, call.problems.is_empty()) {
            (0, _) => CallOutcome::NoTranscript,
            (_, true) => CallOutcome::Recorded,
            (_, false) => CallOutcome::RecordedWithProblems,
        };
        EndedCallView {
            id: call.id.clone(),
            platform: call.platform,
            title: call.title.clone(),
            started_at: call.started_at,
            ended_at: self.ended_at,
            caption_count: call.caption_count,
            outcome,
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
            CallState::Problem(reason) => {
                format!("{name}: not recording, {}; {captions}", reason.describe())
            }
            CallState::Stopped => format!("{name}: recording stopped by you; {captions}"),
        }
    }
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
        format!(
            "{start} to {end}  {name}: {} caption{} in {minutes} min, {outcome}",
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
    loop {
        interval.tick().await;
        let state = app.state::<AppState>();
        let board = state.calls.status();
        let tick = board.tick(Moment::now());
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

fn report_problem_start(app: &AppHandle, start: &ProblemStart) {
    match start.platform {
        CallPlatform::Meet => {
            let state = app.state::<AppState>();
            if start.is_first {
                state.call_debug.write_lead_up(&start.call_id);
            }
            if state.call_debug.take_snapshot_turn(&start.call_id) {
                state.browser_calls.request_page_snapshot(&start.call_id);
            }
        }
        // The huddle log already holds every health change the watcher saw.
        CallPlatform::SlackHuddle => {}
    }
}

fn announce(app: &AppHandle, announced: &Announced) {
    let name = call_name(announced.platform, announced.title.as_deref());
    let source = match announced.platform {
        CallPlatform::Meet => "the Meet call",
        CallPlatform::SlackHuddle => "the Slack huddle",
    };
    let (heading, body) = match announced.announcement {
        Announcement::Recording => (
            format!("Recording {name}"),
            format!("Captions arrive from {source}."),
        ),
        Announcement::NotRecording(reason) => (
            format!("Not recording {name}"),
            capitalise(reason.describe()),
        ),
        Announcement::RecordingAgain => (
            format!("Recording again {name}"),
            format!("Captions arrive from {source} again."),
        ),
        Announcement::NoTranscript => (
            format!("No transcript captured for {name}"),
            "The call ended with no caption. \"Save debug report\" in the tray keeps what \
             happened."
                .to_owned(),
        ),
    };
    crate::notify(app, &heading, &body);
}

fn capitalise(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}
