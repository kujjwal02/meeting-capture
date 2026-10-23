//! One call stream from every call source: each source reports when a call opens and ends, and
//! this logs `call.started`, `call.resumed` and `call.ended` and broadcasts them. The status
//! board hears the same opens and ends, so it never sees a call the stream has not.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use sources::browser_call::EndReason;
use tokio::sync::broadcast;

use crate::call_status::{CallBoard, Moment};

/// Events a slow subscriber may fall behind by before it misses some.
const EVENT_BACKLOG: usize = 64;

/// What capture and the log hear about a call.
#[derive(Debug, Clone)]
pub(crate) enum CallEvent {
    Started(Call),
    /// A call that ended earlier in this run opened again: a Meet rejoin, or late events after an
    /// idle end. It keeps its id.
    Resumed(Call),
    Ended {
        call: Call,
        reason: CallEndReason,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct Call {
    /// The source's log path, which names the call in both sources.
    pub id: PathBuf,
    pub platform: CallPlatform,
    /// The Meet title or meeting code, or the huddle's channel. `None` for a call ended at launch.
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallPlatform {
    Meet,
    SlackHuddle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallEndReason {
    Left,
    TabClosed,
    Kicked,
    PageUnload,
    /// No extension port held the call for 5 minutes: the browser quit or crashed.
    Idle,
    /// The app found the call's log open at launch, after a quit or a crash.
    AppRestarted,
    /// The huddle window closed, or the watcher stopped reading it.
    WindowClosed,
}

/// The open calls and the event sender, shared by every source.
pub(crate) struct Calls {
    tally: Mutex<CallTally>,
    sender: broadcast::Sender<CallEvent>,
    status: CallBoard,
}

#[derive(Default)]
struct CallTally {
    open: HashMap<PathBuf, Call>,
    /// Calls that ended in this run, so a reopen is a resume. Calls ended at launch are not here:
    /// after a restart, the source's next report is a fresh start.
    ended: HashSet<PathBuf>,
}

impl Calls {
    pub fn new() -> Self {
        Self {
            tally: Mutex::new(CallTally::default()),
            sender: broadcast::channel(EVENT_BACKLOG).0,
            status: CallBoard::new(),
        }
    }

    /// Each open call's state, and the calls that ended in this run.
    pub fn status(&self) -> &CallBoard {
        &self.status
    }

    #[expect(dead_code, reason = "phase 3 capture subscribes")]
    pub fn subscribe(&self) -> broadcast::Receiver<CallEvent> {
        self.sender.subscribe()
    }

    /// Reports a call a source has open. Repeats for a call already open do nothing.
    pub fn open(&self, id: &Path, platform: CallPlatform, title: Option<String>) {
        let mut tally = self.lock();
        if tally.open.contains_key(id) {
            return;
        }
        let call = Call {
            id: id.to_path_buf(),
            platform,
            title,
        };
        self.status
            .open(id, platform, call.title.clone(), Moment::now());
        tally.open.insert(call.id.clone(), call.clone());
        let event = if tally.ended.remove(id) {
            CallEvent::Resumed(call)
        } else {
            CallEvent::Started(call)
        };
        // Under the lock, so an end and a resume of one call cannot reach subscribers swapped.
        self.publish(event);
    }

    /// Reports a call a source ended. A call not open in this run is ignored: its end reaches
    /// the source only as a resend after a restart, and launch already reported it.
    pub fn end(&self, id: &Path, reason: CallEndReason) {
        let mut tally = self.lock();
        let Some(call) = tally.open.remove(id) else {
            return;
        };
        tally.ended.insert(call.id.clone());
        self.status.end(id, Moment::now());
        self.publish(CallEvent::Ended { call, reason });
    }

    /// Reports the calls whose logs launch found open and ended.
    pub fn end_at_launch(&self, ids: &[PathBuf], platform: CallPlatform) {
        for id in ids {
            let call = Call {
                id: id.clone(),
                platform,
                title: None,
            };
            self.publish(CallEvent::Ended {
                call,
                reason: CallEndReason::AppRestarted,
            });
        }
    }

    fn lock(&self) -> MutexGuard<'_, CallTally> {
        self.tally.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, event: CallEvent) {
        log(&event);
        // An error only means no one subscribes yet.
        let _unheard = self.sender.send(event);
    }
}

fn log(event: &CallEvent) {
    match event {
        CallEvent::Started(call) => tracing::info!(
            event.name = "call.started",
            call.id = %call.id.display(),
            call.platform = call.platform.as_str(),
            call.title = call.title,
            "call started"
        ),
        CallEvent::Resumed(call) => tracing::info!(
            event.name = "call.resumed",
            call.id = %call.id.display(),
            call.platform = call.platform.as_str(),
            call.title = call.title,
            "call resumed"
        ),
        CallEvent::Ended { call, reason } => tracing::info!(
            event.name = "call.ended",
            call.id = %call.id.display(),
            call.platform = call.platform.as_str(),
            call.title = call.title,
            call.end_reason = reason.as_str(),
            "call ended"
        ),
    }
}

impl CallPlatform {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Meet => "meet",
            Self::SlackHuddle => "slack_huddle",
        }
    }
}

impl CallEndReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::TabClosed => "tab_closed",
            Self::Kicked => "kicked",
            Self::PageUnload => "page_unload",
            Self::Idle => "idle",
            Self::AppRestarted => "app_restarted",
            Self::WindowClosed => "window_closed",
        }
    }
}

impl From<EndReason> for CallEndReason {
    fn from(reason: EndReason) -> Self {
        match reason {
            EndReason::Left => Self::Left,
            EndReason::TabClosed => Self::TabClosed,
            EndReason::Kicked => Self::Kicked,
            EndReason::PageUnload => Self::PageUnload,
            EndReason::Idle => Self::Idle,
            EndReason::AppRestarted => Self::AppRestarted,
        }
    }
}
