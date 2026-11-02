//! Routes lines to each call's detectors, steps the clock through their deadlines, and turns
//! changes of their problems into edges and notifications.

use std::collections::{HashMap, HashSet};

use jiff::{SignedDuration, Timestamp};
use telemetry::events;
use telemetry::parsed::{Entry, Line};

use crate::captions::{CaptionSource, CaptionTally, HuddleCaptions, HuddleHealth, MeetCaptions};
use crate::capture::{AudioHelper, Mic, RemoteAudio, Video};
use crate::context::{CallContext, CallFacts, GlobalContext, Platform, SuspendedSpan};
use crate::problem::Problem;
use crate::reading;
use crate::{Detector, DetectorKind};

/// A problem that lasts this long gets a notification. Shorter ones are reconnects and pauses.
const NOTICE_AFTER: SignedDuration = SignedDuration::from_secs(30);
/// A call's kept problem spans. A flapping device past this adds nothing to the verdict.
const KEPT_SPANS: usize = 64;
/// The evidence of a change that a deadline made, not a line.
const DEADLINE: &str = "deadline";
/// The evidence of a change that a level row made.
const LEVEL_ROW: &str = "mc.audio.level.peak";

/// A call's key: its trace id. Log lines and metric rows of a call both carry it, and a report
/// without text keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceKey(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Raised(Problem),
    /// The detector's problem changed with no clear between.
    Changed {
        from: Problem,
        to: Problem,
    },
    Cleared(Problem),
}

/// The line that changed a verdict: its `event.name`, the metric for a level rule, or
/// `deadline` for a timer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub event: String,
    pub at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub call: TraceKey,
    pub call_id: Option<String>,
    pub at: Timestamp,
    pub detector: DetectorKind,
    pub change: Change,
    pub evidence: Evidence,
}

/// What the user hears about a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The first caption arrived.
    Recording,
    /// A caption problem lasted 30 s. Once per problem in one outage.
    NotRecording(Problem),
    /// A caption arrived after a `NotRecording`. It ends the outage.
    RecordingAgain,
    /// The call ended with no caption.
    NoTranscript,
    /// A capture problem lasted 30 s.
    CaptureTrouble(Problem),
    /// A capture problem cleared after its `CaptureTrouble`.
    CaptureBack(Problem),
}

#[derive(Debug, Clone)]
pub struct CallNotice {
    pub call: TraceKey,
    pub call_id: Option<String>,
    pub at: Timestamp,
    pub notice: Notice,
}

#[derive(Debug, Default)]
pub struct HostOutput {
    pub edges: Vec<Edge>,
    pub notices: Vec<CallNotice>,
}

/// One problem from its raise to its clear.
#[derive(Debug, Clone, Copy)]
pub struct ProblemSpan {
    pub problem: Problem,
    pub detector: DetectorKind,
    pub raised_at: Timestamp,
    pub noticed_at: Option<Timestamp>,
    /// `None` while it lasts.
    pub cleared_at: Option<Timestamp>,
}

/// A call's state as its caption detector sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionState {
    /// The call is open and no caption has arrived yet.
    Waiting,
    /// A caption arrived, and no caption problem is in view.
    Recording,
    Problem(Problem),
    /// The user stopped recording the call.
    Stopped,
}

#[derive(Debug, Clone)]
pub struct CallSnapshot {
    pub call_id: Option<String>,
    pub platform: Option<Platform>,
    pub opened_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub caption_state: CaptionState,
    pub caption_count: u64,
    pub last_caption_at: Option<Timestamp>,
    pub recording_since: Option<Timestamp>,
    pub caption_source: Option<CaptionSource>,
    pub huddle_health: Option<HuddleHealth>,
    pub capture_problems: Vec<Problem>,
    pub history: Vec<ProblemSpan>,
}

enum Captions {
    Meet(MeetCaptions),
    Huddle(HuddleCaptions),
}

impl Captions {
    fn tally(&self) -> &CaptionTally {
        match self {
            Self::Meet(meet) => &meet.tally,
            Self::Huddle(huddle) => &huddle.tally,
        }
    }

    fn detector(&self) -> &dyn Detector {
        match self {
            Self::Meet(meet) => meet,
            Self::Huddle(huddle) => huddle,
        }
    }

    fn detector_mut(&mut self) -> &mut dyn Detector {
        match self {
            Self::Meet(meet) => meet,
            Self::Huddle(huddle) => huddle,
        }
    }
}

struct CallDetectors {
    /// `None` until the platform is known.
    captions: Option<Captions>,
    mic: Mic,
    remote: RemoteAudio,
    audio_helper: AudioHelper,
    video: Video,
}

impl CallDetectors {
    fn new(opened_at: Timestamp) -> Self {
        Self {
            captions: None,
            mic: Mic::new(),
            remote: RemoteAudio::new(opened_at),
            audio_helper: AudioHelper::new(opened_at),
            video: Video::new(opened_at),
        }
    }

    /// Captions first: the order edges of one moment come out in.
    fn each(&self) -> impl Iterator<Item = &dyn Detector> {
        let capture: [&dyn Detector; 4] =
            [&self.mic, &self.remote, &self.audio_helper, &self.video];
        self.captions
            .as_ref()
            .map(Captions::detector)
            .into_iter()
            .chain(capture)
    }

    fn each_mut(&mut self) -> impl Iterator<Item = &mut dyn Detector> {
        let capture: [&mut dyn Detector; 4] = [
            &mut self.mic,
            &mut self.remote,
            &mut self.audio_helper,
            &mut self.video,
        ];
        self.captions
            .as_mut()
            .map(Captions::detector_mut)
            .into_iter()
            .chain(capture)
    }
}

struct ActiveProblem {
    problem: Problem,
    /// When the detector's outage began, kept when its problem changes.
    since: Timestamp,
    is_noticed: bool,
    /// Its span in the history, unless the history was full.
    span: Option<usize>,
}

/// What the call announced, so each notification goes out once, even when a state flips back
/// and forth.
#[derive(Default)]
struct Notices {
    is_recording_announced: bool,
    /// The problems announced in each detector's current outage. A caption outage ends at a
    /// "Recording again"; a capture outage at its clear.
    outages: HashMap<DetectorKind, HashSet<Problem>>,
    /// When the last "Not recording" went out. A caption after it is a recovery.
    recovery_owed_since: Option<Timestamp>,
    is_no_transcript_announced: bool,
}

struct CallSlot {
    call_id: Option<String>,
    opened_at: Timestamp,
    ended_at: Option<Timestamp>,
    facts: CallFacts,
    detectors: CallDetectors,
    active: HashMap<DetectorKind, ActiveProblem>,
    notices: Notices,
    history: Vec<ProblemSpan>,
}

impl CallSlot {
    fn new(opened_at: Timestamp) -> Self {
        Self {
            call_id: None,
            opened_at,
            ended_at: None,
            facts: CallFacts::default(),
            detectors: CallDetectors::new(opened_at),
            active: HashMap::new(),
            notices: Notices::default(),
            history: Vec::new(),
        }
    }

    fn is_open(&self) -> bool {
        self.ended_at.is_none()
    }

    fn learn_platform(&mut self, platform: Platform) {
        if self.facts.platform.is_some() {
            return;
        }
        self.facts.platform = Some(platform);
        self.detectors.captions = Some(match platform {
            Platform::Meet => Captions::Meet(MeetCaptions::new(self.opened_at)),
            Platform::SlackHuddle => Captions::Huddle(HuddleCaptions::new(self.opened_at)),
        });
    }

    fn caption_state(&self) -> CaptionState {
        if self.facts.is_stopped_by_user {
            return CaptionState::Stopped;
        }
        let caption_kind = self
            .detectors
            .captions
            .as_ref()
            .map(|captions| captions.detector().kind());
        let problem = caption_kind
            .and_then(|kind| self.active.get(&kind))
            .map(|active| active.problem);
        match problem {
            Some(problem) => CaptionState::Problem(problem),
            None if self
                .detectors
                .captions
                .as_ref()
                .is_some_and(|captions| captions.tally().has_caption_since_wait()) =>
            {
                CaptionState::Recording
            }
            None => CaptionState::Waiting,
        }
    }

    /// The earliest moment after `clock` at which a detector or a notice could change.
    fn next_deadline(&self, global: &GlobalContext, clock: Timestamp) -> Option<Timestamp> {
        if !self.is_open() {
            return None;
        }
        let context = CallContext {
            facts: &self.facts,
            global,
        };
        let detector_deadlines = self
            .detectors
            .each()
            .filter_map(|detector| detector.next_deadline(&context));
        let notice_deadlines = self
            .active
            .values()
            .filter(|active| {
                !active.is_noticed
                    && active
                        .problem
                        .deserves_notice(global.is_extension_connected)
            })
            .map(|active| context.deadline(active.since, NOTICE_AFTER));
        detector_deadlines
            .chain(notice_deadlines)
            .filter(|deadline| *deadline > clock)
            .min()
    }

    fn take_line(&mut self, line: &Line, event: Option<&str>, global: &GlobalContext) {
        if let Some(call_id) = reading::text(line, events::CALL_ID) {
            self.call_id.get_or_insert_with(|| call_id.to_owned());
        }
        let platform = reading::text(line, events::CALL_PLATFORM)
            .and_then(Platform::parse)
            .or_else(|| match event {
                Some(name) if name.starts_with("meet.") => Some(Platform::Meet),
                Some(name) if name.starts_with("huddle.") => Some(Platform::SlackHuddle),
                Some(_) | None => None,
            });
        if let Some(platform) = platform {
            self.learn_platform(platform);
        }
        let context = CallContext {
            facts: &self.facts,
            global,
        };
        for detector in self.detectors.each_mut() {
            detector.take_line(line, &context);
        }
        match event {
            Some(events::CALL_RECORDING_STOPPED) => self.facts.is_stopped_by_user = true,
            Some(events::CALL_RECORDING_RESUMED) => self.facts.is_stopped_by_user = false,
            Some(events::CAPTURE_AUDIO_SLEEP) => self.facts.is_asleep = true,
            Some(events::CAPTURE_AUDIO_WAKE | events::CAPTURE_AUDIO_RECORDING) => {
                self.facts.is_asleep = false;
            }
            Some(_) | None => {}
        }
    }

    fn advance(&mut self, at: Timestamp, global: &GlobalContext) {
        let context = CallContext {
            facts: &self.facts,
            global,
        };
        for detector in self.detectors.each_mut() {
            detector.advance(at, &context);
        }
    }

    fn open_span(
        &mut self,
        problem: Problem,
        detector: DetectorKind,
        at: Timestamp,
    ) -> Option<usize> {
        if self.history.len() >= KEPT_SPANS {
            return None;
        }
        self.history.push(ProblemSpan {
            problem,
            detector,
            raised_at: at,
            noticed_at: None,
            cleared_at: None,
        });
        Some(self.history.len() - 1)
    }

    fn close_span(&mut self, span: Option<usize>, at: Timestamp) {
        if let Some(span) = span.and_then(|index| self.history.get_mut(index)) {
            span.cleared_at = Some(at);
        }
    }

    /// The call ended: open problems end with it, and nothing is raised until it resumes.
    fn end(&mut self, at: Timestamp) -> Option<Notice> {
        self.ended_at = Some(at);
        let spans: Vec<Option<usize>> =
            self.active.drain().map(|(_, active)| active.span).collect();
        for span in spans {
            self.close_span(span, at);
        }
        let count = self
            .detectors
            .captions
            .as_ref()
            .map_or(0, |captions| captions.tally().count);
        (count == 0 && !self.notices.is_no_transcript_announced).then(|| {
            self.notices.is_no_transcript_announced = true;
            Notice::NoTranscript
        })
    }

    fn resume(&mut self) {
        self.ended_at = None;
        self.notices.outages.clear();
        self.notices.recovery_owed_since = None;
    }

    /// Compares each detector's problem with the active one and records what changed at `at`.
    fn settle(
        &mut self,
        key: &TraceKey,
        at: Timestamp,
        evidence: &Evidence,
        global: &GlobalContext,
        output: &mut HostOutput,
    ) {
        if !self.is_open() {
            return;
        }
        let context = CallContext {
            facts: &self.facts,
            global,
        };
        let verdicts: Vec<(DetectorKind, Option<Problem>)> = self
            .detectors
            .each()
            .map(|detector| (detector.kind(), detector.problem(&context)))
            .collect();
        let mut notices = Vec::new();
        for (kind, current) in verdicts {
            let change = match (self.active.get(&kind).map(|active| active.problem), current) {
                (None, None) => None,
                (Some(active), Some(problem)) if active == problem => None,
                (None, Some(problem)) => {
                    let span = self.open_span(problem, kind, at);
                    self.active.insert(
                        kind,
                        ActiveProblem {
                            problem,
                            since: at,
                            is_noticed: false,
                            span,
                        },
                    );
                    Some(Change::Raised(problem))
                }
                (Some(from), Some(to)) => {
                    let previous = self
                        .active
                        .remove(&kind)
                        .map(|active| (active.since, active.span));
                    let (since, old_span) = previous.unwrap_or((at, None));
                    self.close_span(old_span, at);
                    let span = self.open_span(to, kind, at);
                    self.active.insert(
                        kind,
                        ActiveProblem {
                            problem: to,
                            since,
                            is_noticed: false,
                            span,
                        },
                    );
                    Some(Change::Changed { from, to })
                }
                (Some(problem), None) => {
                    let span = self.active.remove(&kind).and_then(|active| active.span);
                    self.close_span(span, at);
                    Some(Change::Cleared(problem))
                }
            };
            if kind.is_capture() && current.is_none() {
                self.end_capture_outage(kind, &mut notices);
            }
            if let Some(change) = change {
                output.edges.push(Edge {
                    call: key.clone(),
                    call_id: self.call_id.clone(),
                    at,
                    detector: kind,
                    change,
                    evidence: evidence.clone(),
                });
            }
        }
        self.notice_lasting(at, global, &mut notices);
        self.notice_captions(&mut notices);
        output
            .notices
            .extend(notices.into_iter().map(|notice| CallNotice {
                call: key.clone(),
                call_id: self.call_id.clone(),
                at,
                notice,
            }));
    }

    fn notice_lasting(&mut self, at: Timestamp, global: &GlobalContext, notices: &mut Vec<Notice>) {
        let context = CallContext {
            facts: &self.facts,
            global,
        };
        let mut noticed_spans = Vec::new();
        for (kind, active) in &mut self.active {
            let has_lasted = context.elapsed(active.since, at) >= NOTICE_AFTER;
            if active.is_noticed
                || !has_lasted
                || !active
                    .problem
                    .deserves_notice(global.is_extension_connected)
            {
                continue;
            }
            active.is_noticed = true;
            noticed_spans.push(active.span);
            let is_new_in_outage = self
                .notices
                .outages
                .entry(*kind)
                .or_default()
                .insert(active.problem);
            if active.problem.layer().is_capture() {
                if is_new_in_outage {
                    notices.push(Notice::CaptureTrouble(active.problem));
                }
            } else {
                self.notices.recovery_owed_since = Some(at);
                if is_new_in_outage {
                    notices.push(Notice::NotRecording(active.problem));
                }
            }
        }
        for span in noticed_spans.into_iter().flatten() {
            if let Some(span) = self.history.get_mut(span) {
                span.noticed_at = Some(at);
            }
        }
    }

    /// Ends a capture detector's outage once it shows no problem. A sleep hides every capture
    /// problem, so the outage waits for the wake: the problem may still be there, and "back"
    /// would then be false.
    fn end_capture_outage(&mut self, kind: DetectorKind, notices: &mut Vec<Notice>) {
        if self.facts.is_asleep {
            return;
        }
        let Some(outage) = self.notices.outages.remove(&kind) else {
            return;
        };
        // The user's stop is no recovery to announce.
        if self.facts.is_stopped_by_user {
            return;
        }
        // One detector's problems share a medium, which is all the notice names.
        if let Some(problem) = outage.into_iter().min_by_key(|problem| problem.code()) {
            notices.push(Notice::CaptureBack(problem));
        }
    }

    fn notice_captions(&mut self, notices: &mut Vec<Notice>) {
        if self.caption_state() != CaptionState::Recording {
            return;
        }
        let last_caption_at = self
            .detectors
            .captions
            .as_ref()
            .and_then(|captions| captions.tally().last_at);
        // The first "Recording" also tells of a recovery from a problem before it.
        let is_recovered = self
            .notices
            .recovery_owed_since
            .zip(last_caption_at)
            .is_some_and(|(owed_since, caption_at)| caption_at > owed_since);
        let notice = if !self.notices.is_recording_announced {
            self.notices.is_recording_announced = true;
            Notice::Recording
        } else if is_recovered {
            Notice::RecordingAgain
        } else {
            return;
        };
        self.end_caption_outage();
        notices.push(notice);
    }

    // A later outage, even with the same problem, is announced again. The 30 s notice delay
    // keeps short flaps quiet.
    fn end_caption_outage(&mut self) {
        self.notices.outages.retain(|kind, _| {
            !matches!(
                kind,
                DetectorKind::MeetCaptions | DetectorKind::HuddleCaptions
            )
        });
        self.notices.recovery_owed_since = None;
    }

    fn snapshot(&self) -> CallSnapshot {
        let (caption_source, huddle_health) = match &self.detectors.captions {
            Some(Captions::Meet(meet)) => (meet.source, None),
            Some(Captions::Huddle(huddle)) => (None, huddle.health),
            None => (None, None),
        };
        let tally = self.detectors.captions.as_ref().map(Captions::tally);
        let mut capture_problems: Vec<(DetectorKind, Problem)> = self
            .active
            .iter()
            .filter(|(_, active)| active.problem.layer().is_capture())
            .map(|(kind, active)| (*kind, active.problem))
            .collect();
        capture_problems.sort_by_key(|(_, problem)| problem.layer());
        CallSnapshot {
            call_id: self.call_id.clone(),
            platform: self.facts.platform,
            opened_at: self.opened_at,
            ended_at: self.ended_at,
            caption_state: self.caption_state(),
            caption_count: tally.map_or(0, |tally| tally.count),
            last_caption_at: tally.and_then(|tally| tally.last_at),
            recording_since: tally.and_then(|tally| tally.recording_since),
            caption_source,
            huddle_health,
            capture_problems: capture_problems
                .into_iter()
                .map(|(_, problem)| problem)
                .collect(),
            history: self.history.clone(),
        }
    }
}

/// Every call's detectors, one clock, and what changed since the last drain.
#[derive(Default)]
pub struct Host {
    calls: HashMap<TraceKey, CallSlot>,
    /// Calls `forget` dropped. A late line for one would open a slot that no `call.ended`
    /// closes.
    forgotten: HashSet<TraceKey>,
    global: GlobalContext,
    clock: Option<Timestamp>,
    output: HostOutput,
}

impl Host {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One line. A line older than the host's clock counts at the clock.
    pub fn take_line(&mut self, line: &Line) {
        let event = reading::event_name(line);
        let is_row = matches!(line.entry, Entry::Metric(_));
        if !is_row && !event.is_some_and(|name| events::DETECTOR_EVENTS.contains(&name)) {
            return;
        }
        // The span lies before the line, so it must count before the deadlines inside it fire.
        self.learn_suspended([line]);
        let at = self.clock.map_or(line.at, |clock| clock.max(line.at));
        self.step_to(at);
        self.clock = Some(at);
        let evidence = Evidence {
            event: event.unwrap_or(LEVEL_ROW).to_owned(),
            at: line.at,
        };
        match event {
            Some(events::EXTENSION_CONNECTION | events::HOST_SUSPENDED) => {
                if let Some(is_connected) = reading::flag(line, events::EXTENSION_IS_CONNECTED) {
                    self.global.is_extension_connected = is_connected;
                }
                self.settle_all(at, &evidence);
            }
            Some(_) | None => self.take_call_line(line, event, at, &evidence),
        }
    }

    /// Learns the sleeps that a batch's `host.suspended` lines report, before any of its lines
    /// steps the clock. The app logs the line at its first tick after a wake, so lines stamped
    /// earlier in the same batch would otherwise fire the deadlines inside the sleep.
    pub fn learn_suspended<'a>(&mut self, lines: impl IntoIterator<Item = &'a Line>) {
        for line in lines {
            if reading::event_name(line) != Some(events::HOST_SUSPENDED) {
                continue;
            }
            let Some(suspended_ms) = reading::count(line, events::HOST_SUSPENDED_MS) else {
                continue;
            };
            let suspended =
                SignedDuration::from_millis(i64::try_from(suspended_ms).unwrap_or(i64::MAX));
            let span = SuspendedSpan {
                start: line.at.saturating_sub(suspended).unwrap_or(Timestamp::MIN),
                end: line.at,
            };
            if self.global.suspended.contains(&span) {
                continue;
            }
            // Oldest first: `CallContext::deadline` walks the spans in order.
            let index = self
                .global
                .suspended
                .partition_point(|known| known.start <= span.start);
            self.global.suspended.insert(index, span);
        }
    }

    fn take_call_line(
        &mut self,
        line: &Line,
        event: Option<&str>,
        at: Timestamp,
        evidence: &Evidence,
    ) {
        let Some(trace) = line.trace_id() else {
            return;
        };
        let key = TraceKey(trace.to_owned());
        // A call that opens again stopped logs its stop just before `call.started` or
        // `call.resumed`.
        let is_opening = matches!(
            event,
            Some(events::CALL_STARTED | events::CALL_RESUMED | events::CALL_RECORDING_STOPPED)
        );
        if is_opening {
            self.forgotten.remove(&key);
        } else if self.forgotten.contains(&key) {
            return;
        }
        let slot = self
            .calls
            .entry(key.clone())
            .or_insert_with(|| CallSlot::new(at));
        if event == Some(events::CALL_RESUMED) && !slot.is_open() {
            slot.resume();
        }
        slot.take_line(line, event, &self.global);
        if event == Some(events::CALL_ENDED) {
            if let Some(notice) = slot.is_open().then(|| slot.end(at)).flatten() {
                self.output.notices.push(CallNotice {
                    call: key,
                    call_id: slot.call_id.clone(),
                    at,
                    notice,
                });
            }
            return;
        }
        slot.settle(&key, at, evidence, &self.global, &mut self.output);
    }

    /// Moves the clock to `at`, firing every deadline on the way at its own time.
    pub fn advance_to(&mut self, at: Timestamp) {
        let at = self.clock.map_or(at, |clock| clock.max(at));
        self.step_to(at);
        self.clock = Some(at);
        let evidence = Evidence {
            event: DEADLINE.to_owned(),
            at,
        };
        for slot in self.calls.values_mut() {
            slot.advance(at, &self.global);
        }
        self.settle_all(at, &evidence);
    }

    fn step_to(&mut self, until: Timestamp) {
        loop {
            let Some(clock) = self.clock else {
                return;
            };
            let next = self
                .calls
                .iter()
                .filter_map(|(key, slot)| {
                    slot.next_deadline(&self.global, clock)
                        .map(|deadline| (deadline, key))
                })
                .filter(|(deadline, _)| *deadline <= until)
                .min_by_key(|(deadline, _)| *deadline);
            let Some((deadline, key)) = next else {
                return;
            };
            let key = key.clone();
            self.clock = Some(deadline);
            if let Some(slot) = self.calls.get_mut(&key) {
                slot.advance(deadline, &self.global);
                let evidence = Evidence {
                    event: DEADLINE.to_owned(),
                    at: deadline,
                };
                slot.settle(&key, deadline, &evidence, &self.global, &mut self.output);
            }
        }
    }

    fn settle_all(&mut self, at: Timestamp, evidence: &Evidence) {
        for (key, slot) in &mut self.calls {
            slot.settle(key, at, evidence, &self.global, &mut self.output);
        }
    }

    /// Edges and notices since the last drain, oldest first.
    pub fn drain_output(&mut self) -> HostOutput {
        std::mem::take(&mut self.output)
    }

    #[must_use]
    pub fn snapshot(&self, call: &TraceKey) -> Option<CallSnapshot> {
        self.calls.get(call).map(CallSlot::snapshot)
    }

    /// Every call the host knows, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<TraceKey> {
        let mut calls: Vec<(&TraceKey, Timestamp)> = self
            .calls
            .iter()
            .map(|(key, slot)| (key, slot.opened_at))
            .collect();
        calls.sort_by_key(|(key, opened_at)| (*opened_at, (*key).clone()));
        calls.into_iter().map(|(key, _)| key.clone()).collect()
    }

    /// The host's clock: the last line or step.
    #[must_use]
    pub fn clock(&self) -> Option<Timestamp> {
        self.clock
    }

    /// Drops a call nothing reports on again.
    pub fn forget(&mut self, call: &TraceKey) {
        self.calls.remove(call);
        self.forgotten.insert(call.clone());
    }
}

/// When a span ends for the verdict: its clear, else `fallback`.
pub(crate) fn span_end(span: &ProblemSpan, fallback: Timestamp) -> Timestamp {
    span.cleared_at.unwrap_or(fallback).max(span.raised_at)
}
