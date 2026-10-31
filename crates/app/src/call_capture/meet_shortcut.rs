//! Starts a Meet call's tab video with no click. Chrome hands out a tab's stream only after an
//! invocation of mc-meet on the tab, and a `commands` shortcut is one. So when mc-meet reports
//! `needs_click`, the app presses mc-meet's shortcut into the Chrome that hosts it, as a real
//! key event. mc-meet takes any press on a tab that needs the invocation as an arm, never as
//! the user's stop, so the app's press cannot stop a recording.
//!
//! The press goes to Chrome's pid, so it never types into another app, and it works while
//! Chrome is behind another app: Chrome takes a key posted to its pid and runs the command on
//! the active tab of its last focused window. So the app presses whenever the Meet tab is that
//! tab, with no focus change. It needs the Accessibility grant, which the user gives by hand;
//! the app never raises the prompt.
//!
//! Every Chrome profile shares one browser pid, and mc-meet cannot see the windows of another
//! profile, an app window or incognito. A press can land in one of them. So the shortcut must
//! hold Command or Control, which type no text there, and mc-meet reports a press it never
//! saw. Every welcomed mc-meet hears the press notice, so one in another profile leaves the
//! command alone and reports where it landed instead of taking it as the user's stop. After
//! one missed press the app stops pressing for the call. The click on the mc-meet icon stays
//! the fallback whenever a condition fails.

use std::collections::HashSet;
use std::time::Duration;

use browser_shortcut::{PressError, Readiness, Shortcut, ShortcutError};
use tokio::time::Instant;

use super::VideoState;
use crate::browser_calls::{BrowserCalls, PressChance, PressMiss};

/// At most one press per this long, so a run of focus changes does not hammer Chrome.
const PRESS_INTERVAL: Duration = Duration::from_secs(3);
/// Every welcomed mc-meet answers `shortcut_press_expected` within this long, or the press is
/// skipped. So the press goes out at most this long after the notice. mc-meet's press windows
/// last 2 s from the notice, which leaves the key at least 1 s to arrive.
const REPLY_LIMIT: Duration = Duration::from_secs(1);
/// An unseen report this long after the last press is about that press. mc-meet reports 2 s
/// after its ready reply, and a ready the app skipped has no press to blame.
const UNSEEN_MATCH_LIMIT: Duration = Duration::from_secs(3);
/// Presses that opened no stream before the app gives up on the route until the next capture.
/// A press that never takes would otherwise repeat on every focus change.
const MAX_PRESSES: u32 = 3;
/// The browsers whose pid may get the press: Chromium builds that run mc-meet. The pid comes
/// from the host's hello, which any process of the user could send, so a pid of another app
/// gets no key.
const CHROMIUM_BUNDLE_IDS: [&str; 5] = [
    "com.google.Chrome",
    "com.google.Chrome.beta",
    "com.google.Chrome.dev",
    "com.google.Chrome.canary",
    "org.chromium.Chromium",
];

/// Why the app did not press, as the log and the tray tell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressSkip {
    /// The call's video is stopping.
    Stopping,
    Unsupported,
    AccessibilityMissing,
    /// mc-meet sent no press facts.
    ExtensionOutdated,
    ShortcutUnbound,
    ShortcutUnmappable,
    /// The shortcut holds neither Command nor Control.
    ShortcutTypesText,
    ShortcutNotOnLayout,
    /// The host did not say which browser started it.
    BrowserUnknown,
    /// The host's pid is not a Chromium browser.
    BrowserNotChromium,
    MeetTabInactive,
    /// Another Chrome window was focused after the Meet tab's window.
    WindowNotLastFocused,
    TooSoon,
    PressLimit,
    NoReply,
    /// mc-meet read the facts again before its reply, and they failed.
    PressRefused,
    /// An earlier press reached no mc-meet.
    PressUnseen,
    PressFailed,
}

impl PressSkip {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stopping => "stopping",
            Self::Unsupported => "unsupported",
            Self::AccessibilityMissing => "accessibility_missing",
            Self::ExtensionOutdated => "extension_outdated",
            Self::ShortcutUnbound => "shortcut_unbound",
            Self::ShortcutUnmappable => "shortcut_unmappable",
            Self::ShortcutTypesText => "shortcut_types_text",
            Self::ShortcutNotOnLayout => "shortcut_not_on_layout",
            Self::BrowserUnknown => "browser_unknown",
            Self::BrowserNotChromium => "browser_not_chromium",
            Self::MeetTabInactive => "meet_tab_inactive",
            Self::WindowNotLastFocused => "window_not_last_focused",
            Self::TooSoon => "too_soon",
            Self::PressLimit => "press_limit",
            Self::NoReply => "no_reply",
            Self::PressRefused => "press_refused",
            Self::PressUnseen => "press_unseen",
            Self::PressFailed => "press_failed",
        }
    }

    /// The tray row while this holds, or `None` to leave it.
    fn video_state(self) -> Option<VideoState> {
        match self {
            Self::Stopping => None,
            Self::Unsupported
            | Self::BrowserUnknown
            | Self::BrowserNotChromium
            | Self::PressLimit
            | Self::PressFailed => Some(VideoState::TabNeedsClick),
            Self::AccessibilityMissing => Some(VideoState::TabNeedsAccessibility),
            Self::ExtensionOutdated => Some(VideoState::TabExtensionOutdated),
            Self::ShortcutUnbound => Some(VideoState::TabShortcutUnbound),
            Self::ShortcutUnmappable => Some(VideoState::TabShortcutUnmappable),
            Self::ShortcutTypesText => Some(VideoState::TabShortcutTypesText),
            Self::ShortcutNotOnLayout => Some(VideoState::TabShortcutNotOnLayout),
            Self::PressUnseen => Some(VideoState::TabPressUnseen),
            Self::MeetTabInactive
            | Self::WindowNotLastFocused
            | Self::TooSoon
            | Self::PressRefused => Some(VideoState::TabAwaitingFront),
            Self::NoReply => Some(VideoState::TabPressUnanswered),
        }
    }

    /// Holds for the rest of the call, so the log has it once.
    fn is_final(self) -> bool {
        match self {
            Self::PressLimit | Self::PressUnseen => true,
            Self::Stopping
            | Self::Unsupported
            | Self::AccessibilityMissing
            | Self::ExtensionOutdated
            | Self::ShortcutUnbound
            | Self::ShortcutUnmappable
            | Self::ShortcutTypesText
            | Self::ShortcutNotOnLayout
            | Self::BrowserUnknown
            | Self::BrowserNotChromium
            | Self::MeetTabInactive
            | Self::WindowNotLastFocused
            | Self::TooSoon
            | Self::NoReply
            | Self::PressRefused
            | Self::PressFailed => false,
        }
    }
}

/// A press waiting for mc-meet's answers: `shortcut_press_ready` from the worker that holds
/// the session and `shortcut_press_noted` from every other welcomed one.
struct AwaitedPress {
    shortcut: Shortcut,
    shortcut_text: String,
    reply_deadline: Instant,
    /// The welcomed ports at the notice that have not answered yet.
    unanswered_port_ids: HashSet<u64>,
    ready: Option<ReadyReply>,
}

/// The target worker's ready answer.
struct ReadyReply {
    /// The browser of the port that sent it, which is where the press goes.
    browser_pid: Option<u32>,
}

/// One call's presses. The tab video task feeds it mc-meet's reports and its own deadlines.
pub(super) struct MeetShortcut {
    call_id: String,
    /// The last `needs_click` facts, for the retry after a press that opened nothing.
    last_chance: Option<PressChance>,
    awaited: Option<AwaitedPress>,
    last_sent_at: Option<Instant>,
    /// Presses since the last stream opened.
    sent_count: u32,
    /// When to look at `last_chance` again.
    retry_at: Option<Instant>,
    /// A press of this call reached no mc-meet. Kept for the call: the next press would land in
    /// the same unseen window.
    is_press_unseen: bool,
    /// The final skip, once the log has it.
    logged_final_skip: Option<PressSkip>,
}

impl MeetShortcut {
    pub(super) fn new(call_id: String) -> Self {
        Self {
            call_id,
            last_chance: None,
            awaited: None,
            last_sent_at: None,
            sent_count: 0,
            retry_at: None,
            is_press_unseen: false,
            logged_final_skip: None,
        }
    }

    /// The next time `on_deadline` has work.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        match (&self.awaited, self.retry_at) {
            (Some(awaited), Some(retry_at)) => Some(awaited.reply_deadline.min(retry_at)),
            (Some(awaited), None) => Some(awaited.reply_deadline),
            (None, retry_at) => retry_at,
        }
    }

    /// mc-meet's `needs_click`, at the stream refusal or after a focus change of the Meet tab.
    /// Returns the tray row to show.
    pub(super) fn on_needs_click(
        &mut self,
        chance: PressChance,
        is_stopping: bool,
        calls: &BrowserCalls,
    ) -> Option<VideoState> {
        self.last_chance = Some(chance);
        self.consider(is_stopping, calls)
    }

    /// A stream opened, so the tab is armed. A later refusal starts a fresh round of presses.
    pub(super) fn on_stream_open(&mut self) {
        self.last_chance = None;
        self.awaited = None;
        self.retry_at = None;
        self.sent_count = 0;
    }

    /// mc-meet found the Meet tab where the press lands. The app presses once every other
    /// welcomed mc-meet has noted the notice too.
    pub(super) fn on_ready(
        &mut self,
        browser_pid: Option<u32>,
        port_id: u64,
        is_stopping: bool,
    ) -> Option<VideoState> {
        let awaited = self.awaited.as_mut()?;
        awaited.unanswered_port_ids.remove(&port_id);
        awaited.ready = Some(ReadyReply { browser_pid });
        self.press_if_answered(is_stopping)
    }

    /// Another mc-meet heard the notice and leaves the next command alone.
    pub(super) fn on_noted(&mut self, port_id: u64, is_stopping: bool) -> Option<VideoState> {
        self.awaited.as_mut()?.unanswered_port_ids.remove(&port_id);
        self.press_if_answered(is_stopping)
    }

    /// Presses into the browser whose port sent ready, once every port answered. The call may
    /// have stopped and the grant may have gone in the wait, so both are read again.
    fn press_if_answered(&mut self, is_stopping: bool) -> Option<VideoState> {
        let is_answered = self.awaited.as_ref().is_some_and(|awaited| {
            awaited.ready.is_some() && awaited.unanswered_port_ids.is_empty()
        });
        if !is_answered {
            return None;
        }
        let awaited = self.awaited.take()?;
        let now = Instant::now();
        // mc-meet's press windows count from the notice, so a late press could outlive them.
        if awaited.reply_deadline <= now {
            self.retry_at = Some(now + PRESS_INTERVAL);
            return self.skip(PressSkip::NoReply, Some(awaited.shortcut_text.as_str()));
        }
        let browser_pid = awaited.ready.as_ref().and_then(|ready| ready.browser_pid);
        let shortcut_text = Some(awaited.shortcut_text.as_str());
        if is_stopping {
            return self.skip(PressSkip::Stopping, shortcut_text);
        }
        if let Err(skip) = readiness_skip() {
            return self.skip(skip, shortcut_text);
        }
        let Some(browser_pid) = browser_pid else {
            return self.skip(PressSkip::BrowserUnknown, shortcut_text);
        };
        let bundle_id = browser_shortcut::bundle_id_of(browser_pid);
        if !bundle_id
            .as_deref()
            .is_some_and(|id| CHROMIUM_BUNDLE_IDS.contains(&id))
        {
            tracing::warn!(
                call.id = self.call_id,
                process.pid = browser_pid,
                video.browser_bundle_id = bundle_id,
                "video.shortcut_browser_refused"
            );
            return self.skip(PressSkip::BrowserNotChromium, shortcut_text);
        }
        // Logged only: Chrome takes the press from behind another app too.
        let frontmost_pid = browser_shortcut::frontmost_pid();
        self.last_sent_at = Some(now);
        self.sent_count += 1;
        self.retry_at = Some(now + PRESS_INTERVAL);
        match browser_shortcut::press(browser_pid, awaited.shortcut) {
            Ok(()) => {
                tracing::info!(
                    call.id = self.call_id,
                    process.pid = browser_pid,
                    video.browser_bundle_id = bundle_id,
                    video.frontmost_pid = frontmost_pid,
                    video.shortcut = awaited.shortcut_text,
                    video.press_count = self.sent_count,
                    "video.shortcut_sent"
                );
                Some(VideoState::TabWaiting)
            }
            Err(error) => {
                tracing::warn!(
                    call.id = self.call_id,
                    process.pid = browser_pid,
                    exception.message = %error,
                    "video.shortcut_press_failed"
                );
                let skip = match error {
                    PressError::KeyNotOnLayout { .. } => PressSkip::ShortcutNotOnLayout,
                    PressError::Unsupported
                    | PressError::InvalidPid { .. }
                    | PressError::EventNotCreated => PressSkip::PressFailed,
                };
                self.skip(skip, shortcut_text)
            }
        }
    }

    /// mc-meet found the Meet tab moved before it could reply. Its new facts came first; the
    /// app looks at them again after the interval.
    pub(super) fn on_refused(&mut self) -> Option<VideoState> {
        let awaited = self.awaited.take()?;
        self.retry_at = Some(Instant::now() + PRESS_INTERVAL);
        self.skip(PressSkip::PressRefused, Some(&awaited.shortcut_text))
    }

    /// A press missed the Meet tab: it went to a Chrome window the target mc-meet cannot see,
    /// or another profile's mc-meet caught it. The next would land there too, so the app stops
    /// pressing for the call.
    pub(super) fn on_unseen(&mut self, miss: PressMiss) -> Option<VideoState> {
        if self
            .last_sent_at
            .is_none_or(|sent_at| sent_at.elapsed() >= UNSEEN_MATCH_LIMIT)
        {
            // The app skipped the press after mc-meet's ready, so nothing went astray.
            tracing::info!(
                call.id = self.call_id,
                video.press_miss = miss.as_str(),
                "video.shortcut_unseen_ignored"
            );
            return None;
        }
        self.is_press_unseen = true;
        self.retry_at = None;
        tracing::warn!(
            call.id = self.call_id,
            video.press_count = self.sent_count,
            video.press_miss = miss.as_str(),
            "video.shortcut_unseen"
        );
        Some(VideoState::TabPressUnseen)
    }

    /// A reply that did not come, or a retry that is due.
    pub(super) fn on_deadline(
        &mut self,
        is_stopping: bool,
        calls: &BrowserCalls,
    ) -> Option<VideoState> {
        let now = Instant::now();
        if let Some(awaited) = self
            .awaited
            .take_if(|awaited| awaited.reply_deadline <= now)
        {
            tracing::info!(
                call.id = self.call_id,
                video.is_ready_answered = awaited.ready.is_some(),
                video.unanswered_ports = awaited.unanswered_port_ids.len(),
                "video.shortcut_press_unanswered"
            );
            // A worker waking slowly may answer the next notice, so the app asks again.
            self.retry_at = Some(now + PRESS_INTERVAL);
            return self.skip(PressSkip::NoReply, Some(&awaited.shortcut_text));
        }
        if self.retry_at.is_some_and(|retry_at| retry_at <= now) {
            self.retry_at = None;
            return self.consider(is_stopping, calls);
        }
        None
    }

    fn consider(&mut self, is_stopping: bool, calls: &BrowserCalls) -> Option<VideoState> {
        let chance = self.last_chance.clone()?;
        if self.awaited.is_some() {
            return None;
        }
        let shortcut_text = chance.shortcut.as_deref();
        let shortcut = match self.check(&chance, is_stopping) {
            Ok(shortcut) => shortcut,
            Err(skip) => {
                if skip == PressSkip::TooSoon {
                    self.retry_at = self.last_sent_at.map(|at| at + PRESS_INTERVAL);
                }
                return self.skip(skip, shortcut_text);
            }
        };
        tracing::debug!(
            call.id = self.call_id,
            browser_call.session_id = chance.session_id,
            "video.shortcut_press_expected"
        );
        let unanswered_port_ids = calls.expect_shortcut_press(&chance.session_id);
        self.awaited = Some(AwaitedPress {
            shortcut,
            shortcut_text: chance.shortcut.unwrap_or_default(),
            reply_deadline: Instant::now() + REPLY_LIMIT,
            unanswered_port_ids,
            ready: None,
        });
        Some(VideoState::TabWaiting)
    }

    /// Every condition before the app asks mc-meet: the call wants video, the app may send
    /// keys, the shortcut maps to a key that types no text, the Meet tab is where the press
    /// lands, and the rate allows it.
    fn check(&self, chance: &PressChance, is_stopping: bool) -> Result<Shortcut, PressSkip> {
        if is_stopping {
            return Err(PressSkip::Stopping);
        }
        if self.is_press_unseen {
            return Err(PressSkip::PressUnseen);
        }
        if self.sent_count >= MAX_PRESSES {
            return Err(PressSkip::PressLimit);
        }
        readiness_skip()?;
        let (Some(meet_tab), Some(shortcut_text)) = (chance.meet_tab, chance.shortcut.as_deref())
        else {
            return Err(PressSkip::ExtensionOutdated);
        };
        let shortcut = Shortcut::from_chrome(shortcut_text).map_err(|error| match error {
            ShortcutError::Unbound => PressSkip::ShortcutUnbound,
            ShortcutError::UnknownKey { .. }
            | ShortcutError::UnknownModifier { .. }
            | ShortcutError::NoModifier { .. } => PressSkip::ShortcutUnmappable,
        })?;
        if !shortcut.is_text_free() {
            return Err(PressSkip::ShortcutTypesText);
        }
        // Chrome gives the invocation to the active tab of its last focused window, so a press
        // anywhere else would arm the wrong tab.
        if !meet_tab.is_tab_active {
            return Err(PressSkip::MeetTabInactive);
        }
        if !meet_tab.is_window_last_focused {
            return Err(PressSkip::WindowNotLastFocused);
        }
        if let Some(last_sent_at) = self.last_sent_at
            && last_sent_at.elapsed() < PRESS_INTERVAL
        {
            return Err(PressSkip::TooSoon);
        }
        Ok(shortcut)
    }

    fn skip(&mut self, skip: PressSkip, shortcut_text: Option<&str>) -> Option<VideoState> {
        if skip.is_final() && self.logged_final_skip == Some(skip) {
            return skip.video_state();
        }
        if skip.is_final() {
            self.logged_final_skip = Some(skip);
        }
        tracing::info!(
            call.id = self.call_id,
            video.shortcut = shortcut_text,
            video.press_skip_reason = skip.as_str(),
            video.press_count = self.sent_count,
            "video.shortcut_skipped"
        );
        skip.video_state()
    }
}

fn readiness_skip() -> Result<(), PressSkip> {
    match browser_shortcut::readiness() {
        Readiness::Ready => Ok(()),
        Readiness::AccessibilityMissing => Err(PressSkip::AccessibilityMissing),
        Readiness::Unsupported => Err(PressSkip::Unsupported),
    }
}
