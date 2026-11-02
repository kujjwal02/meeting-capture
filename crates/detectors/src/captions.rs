//! The caption rules: whether a Meet call or a Slack huddle gets captions, and if not, why.

use jiff::{SignedDuration, Timestamp};
use telemetry::events;
use telemetry::parsed::Line;

use crate::context::CallContext;
use crate::problem::Problem;
use crate::reading;
use crate::{Detector, DetectorKind};

/// With a cause in view, such as captions off, no caption for this long is a problem. The
/// extension needs a few seconds to turn captions on at call start.
const CAPTION_STALL: SignedDuration = SignedDuration::from_secs(20);
/// No caption for this long while captions are on is a problem by itself. People can be silent
/// for a minute, at the start of a call or during a screen share.
const SILENCE_LIMIT: SignedDuration = SignedDuration::from_mins(2);

/// What Meet's captions button says, from `mc.meet.captions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeetCaptionsShown {
    On,
    Off,
    /// Meet refuses captions: the button is disabled or says they are unavailable.
    Unavailable,
}

impl MeetCaptionsShown {
    fn parse(code: &str) -> Option<Self> {
        match code {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            "unavailable" => Some(Self::Unavailable),
            _unknown => None,
        }
    }
}

/// Where mc-meet reads captions from, from `mc.meet.caption_source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionSource {
    Channel,
    Dom,
    None,
}

impl CaptionSource {
    fn parse(code: &str) -> Option<Self> {
        match code {
            "channel" => Some(Self::Channel),
            "dom" => Some(Self::Dom),
            "none" => Some(Self::None),
            _unknown => None,
        }
    }
}

/// The huddle reader's verdict on the window, from `mc.huddle.health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HuddleHealth {
    Healthy,
    Degraded,
    CaptionsOff,
    Broken,
    Untested,
}

impl HuddleHealth {
    fn parse(code: &str) -> Option<Self> {
        match code {
            "healthy" => Some(Self::Healthy),
            "degraded" => Some(Self::Degraded),
            "captions_off" => Some(Self::CaptionsOff),
            "broken" => Some(Self::Broken),
            "untested" => Some(Self::Untested),
            _unknown => None,
        }
    }
}

/// The captions a call got, from its `call.captions` lines.
#[derive(Debug, Clone)]
pub struct CaptionTally {
    pub count: u64,
    /// When the latest caption reached the app.
    pub last_at: Option<Timestamp>,
    /// Since when the call waits for a caption: its open, its resume, or the user's resume.
    pub waiting_since: Timestamp,
    /// The first caption, or the first one after the last problem.
    pub recording_since: Option<Timestamp>,
}

impl CaptionTally {
    fn new(opened_at: Timestamp) -> Self {
        Self {
            count: 0,
            last_at: None,
            waiting_since: opened_at,
            recording_since: None,
        }
    }

    /// Whether a caption arrived since the call opened or resumed.
    #[must_use]
    pub fn has_caption_since_wait(&self) -> bool {
        self.last_at.is_some_and(|at| at >= self.waiting_since)
    }

    fn quiet_since(&self) -> Timestamp {
        self.last_at
            .map_or(self.waiting_since, |at| at.max(self.waiting_since))
    }

    fn wait_again(&mut self, at: Timestamp) {
        self.waiting_since = at;
        self.recording_since = None;
    }

    /// Takes a `call.captions` line. `has_problem` is the detector's verdict before it.
    fn take(&mut self, line: &Line, has_problem: bool) {
        if let Some(count) = reading::count(line, events::CALL_CAPTION_COUNT) {
            self.count = self.count.max(count);
        }
        // A reopened log's backlog counts, and says nothing about whether captions arrive now.
        if reading::flag(line, events::CAPTION_IS_BACKLOG) == Some(true) {
            return;
        }
        let at = reading::time(line, events::CAPTION_LAST_AT).unwrap_or(line.at);
        if self.recording_since.is_none() || has_problem {
            self.recording_since = Some(at);
        }
        self.last_at = Some(self.last_at.map_or(at, |last| last.max(at)));
    }

    /// The earliest of `limits` after `now`, counted from the quiet start.
    fn next_quiet_deadline(
        &self,
        limits: &[SignedDuration],
        now: Timestamp,
        context: &CallContext<'_>,
    ) -> Option<Timestamp> {
        let since = self.quiet_since();
        limits
            .iter()
            .map(|limit| context.deadline(since, *limit))
            .find(|deadline| *deadline > now)
    }
}

/// Meet's caption rules.
pub struct MeetCaptions {
    pub tally: CaptionTally,
    is_port_held: bool,
    is_pages_away: bool,
    shown: MeetCaptionsShown,
    is_caption_frames_failing: bool,
    pub source: Option<CaptionSource>,
    now: Timestamp,
}

impl MeetCaptions {
    #[must_use]
    pub fn new(opened_at: Timestamp) -> Self {
        Self {
            tally: CaptionTally::new(opened_at),
            is_port_held: true,
            is_pages_away: false,
            shown: MeetCaptionsShown::Off,
            is_caption_frames_failing: false,
            source: None,
            now: opened_at,
        }
    }

    // What the old page said about captions no longer holds on a rejoin.
    fn forget_page(&mut self) {
        self.is_port_held = true;
        self.is_pages_away = false;
        self.shown = MeetCaptionsShown::Off;
        self.is_caption_frames_failing = false;
        self.source = None;
    }
}

impl Detector for MeetCaptions {
    fn kind(&self) -> DetectorKind {
        DetectorKind::MeetCaptions
    }

    fn take_line(&mut self, line: &Line, context: &CallContext<'_>) {
        self.now = self.now.max(line.at);
        match reading::event_name(line) {
            Some(events::CALL_CAPTIONS) => {
                let has_problem = self.problem(context).is_some();
                self.tally.take(line, has_problem);
            }
            Some(events::MEET_HEALTH) => {
                if let Some(shown) =
                    reading::text(line, events::MEET_CAPTIONS).and_then(MeetCaptionsShown::parse)
                {
                    self.shown = shown;
                }
                if let Some(source) =
                    reading::text(line, events::MEET_CAPTION_SOURCE).and_then(CaptionSource::parse)
                {
                    self.source = Some(source);
                }
                if let Some(is_failing) =
                    reading::flag(line, events::MEET_IS_CAPTION_FRAMES_FAILING)
                {
                    self.is_caption_frames_failing = is_failing;
                }
            }
            Some(events::MEET_PORT_HELD) => {
                if let Some(is_held) = reading::flag(line, events::MEET_IS_PORT_HELD) {
                    self.is_port_held = is_held;
                }
            }
            Some(events::MEET_PAGES_AWAY) => {
                if let Some(is_away) = reading::flag(line, events::MEET_IS_PAGES_AWAY) {
                    self.is_pages_away = is_away;
                }
            }
            Some(events::CALL_RESUMED) => {
                self.tally.wait_again(self.now);
                self.forget_page();
            }
            // A resume waits for a fresh caption, as a rejoin does.
            Some(events::CALL_RECORDING_RESUMED) if context.facts.is_stopped_by_user => {
                self.tally.wait_again(self.now);
            }
            Some(_) | None => {}
        }
    }

    fn advance(&mut self, at: Timestamp, _context: &CallContext<'_>) {
        self.now = self.now.max(at);
    }

    fn next_deadline(&self, context: &CallContext<'_>) -> Option<Timestamp> {
        if context.facts.is_stopped_by_user || !self.is_port_held {
            return None;
        }
        self.tally
            .next_quiet_deadline(&[CAPTION_STALL, SILENCE_LIMIT], self.now, context)
    }

    fn problem(&self, context: &CallContext<'_>) -> Option<Problem> {
        if context.facts.is_stopped_by_user {
            return None;
        }
        if !self.is_port_held {
            return Some(Problem::ExtensionDisconnected);
        }
        let quiet_for = context.elapsed(self.tally.quiet_since(), self.now);
        if quiet_for < CAPTION_STALL {
            return None;
        }
        if self.is_pages_away {
            return Some(Problem::MeetPageAway);
        }
        // mc-meet hides captions once a caption arrived on its own channels, which keep
        // streaming. Only then is "off" no cause. mc-meet reports the channel source only after
        // a caption on the channels the page has now.
        let is_off_a_cause = self.source != Some(CaptionSource::Channel);
        // The DOM reader does not need the frames.
        let is_frames_failing =
            self.is_caption_frames_failing && self.source != Some(CaptionSource::Dom);
        // Meet's button can read "unavailable" while mc-meet's channels keep streaming captions,
        // so it is a cause only when the channel is not the source.
        match self.shown {
            MeetCaptionsShown::Unavailable if is_off_a_cause => {
                Some(Problem::MeetCaptionsUnavailable)
            }
            MeetCaptionsShown::On | MeetCaptionsShown::Off | MeetCaptionsShown::Unavailable
                if is_frames_failing =>
            {
                Some(Problem::CaptionParseFailures)
            }
            MeetCaptionsShown::Off if is_off_a_cause => Some(Problem::MeetCaptionsOff),
            MeetCaptionsShown::On | MeetCaptionsShown::Off | MeetCaptionsShown::Unavailable => {
                (quiet_for >= SILENCE_LIMIT).then_some(Problem::CaptionsSilent)
            }
        }
    }
}

/// A Slack huddle's caption rules.
pub struct HuddleCaptions {
    pub tally: CaptionTally,
    pub health: Option<HuddleHealth>,
    now: Timestamp,
}

impl HuddleCaptions {
    #[must_use]
    pub fn new(opened_at: Timestamp) -> Self {
        Self {
            tally: CaptionTally::new(opened_at),
            health: None,
            now: opened_at,
        }
    }
}

impl Detector for HuddleCaptions {
    fn kind(&self) -> DetectorKind {
        DetectorKind::HuddleCaptions
    }

    fn take_line(&mut self, line: &Line, context: &CallContext<'_>) {
        self.now = self.now.max(line.at);
        match reading::event_name(line) {
            Some(events::CALL_CAPTIONS) => {
                let has_problem = self.problem(context).is_some();
                self.tally.take(line, has_problem);
            }
            Some(events::HUDDLE_HEALTH) => {
                if let Some(health) =
                    reading::text(line, events::HUDDLE_HEALTH_FIELD).and_then(HuddleHealth::parse)
                {
                    self.health = Some(health);
                }
            }
            Some(events::CALL_RESUMED) => self.tally.wait_again(self.now),
            Some(events::CALL_RECORDING_RESUMED) if context.facts.is_stopped_by_user => {
                self.tally.wait_again(self.now);
            }
            Some(_) | None => {}
        }
    }

    fn advance(&mut self, at: Timestamp, _context: &CallContext<'_>) {
        self.now = self.now.max(at);
    }

    fn next_deadline(&self, context: &CallContext<'_>) -> Option<Timestamp> {
        if context.facts.is_stopped_by_user {
            return None;
        }
        match self.health {
            Some(HuddleHealth::CaptionsOff | HuddleHealth::Broken) => None,
            Some(HuddleHealth::Healthy | HuddleHealth::Degraded | HuddleHealth::Untested)
            | None => self
                .tally
                .next_quiet_deadline(&[SILENCE_LIMIT], self.now, context),
        }
    }

    fn problem(&self, context: &CallContext<'_>) -> Option<Problem> {
        if context.facts.is_stopped_by_user {
            return None;
        }
        let is_silent = context.elapsed(self.tally.quiet_since(), self.now) >= SILENCE_LIMIT;
        match self.health {
            Some(HuddleHealth::CaptionsOff) => Some(Problem::HuddleCaptionsOff),
            Some(HuddleHealth::Broken) => Some(Problem::HuddleCaptionsUnreadable),
            Some(HuddleHealth::Healthy | HuddleHealth::Degraded) => {
                is_silent.then_some(Problem::CaptionsSilent)
            }
            // No verdict on the window yet, or a Slack version no profile was tested on.
            Some(HuddleHealth::Untested) | None => is_silent.then_some(Problem::NoCaptions),
        }
    }
}
