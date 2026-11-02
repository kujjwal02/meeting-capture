//! The rules that decide whether a call records, over parsed telemetry lines only. The app runs
//! them live on the lines it logs; `mc debug replay` runs them over a saved bundle, so both name
//! the same problems at the same times.
//!
//! A [`Detector`] owns one rule set for one call. The [`Host`] routes each line to its call's
//! detectors, owns the clock, turns changes of each detector's problem into [`Edge`]s, and decides
//! the notifications. [`verdict`] names the layer that failed a call.
//!
//! The crate is portable: no Tauri and no OS calls.

mod captions;
mod capture;
mod context;
mod host;
mod problem;
pub mod problem_lines;
mod queue;
mod reading;
mod verdict;

use jiff::Timestamp;
use telemetry::parsed::Line;

pub use captions::{
    CaptionSource, CaptionTally, HuddleCaptions, HuddleHealth, MeetCaptions, MeetCaptionsShown,
};
pub use capture::{AudioHelper, Mic, RemoteAudio, Video};
pub use context::{CallContext, CallFacts, GlobalContext, Platform, SuspendedSpan};
pub use host::{
    CallNotice, CallSnapshot, CaptionState, Change, Edge, Evidence, Host, HostOutput, Notice,
    OpenProblem, ProblemSpan, TraceKey,
};
pub use problem::{Layer, Problem};
pub use queue::LineQueue;
pub use verdict::{Verdict, VerdictPart, verdict};

/// One rule set over one call's lines. It owns its state; the host owns time and transitions.
pub trait Detector: Send {
    fn kind(&self) -> DetectorKind;
    /// One line of this call. Lines arrive in time order.
    fn take_line(&mut self, line: &Line, context: &CallContext<'_>);
    /// The clock moved to `at`. Time-based rules use it.
    fn advance(&mut self, at: Timestamp, context: &CallContext<'_>);
    /// The earliest time after the detector's clock at which `advance` could change
    /// `problem()`. `None`: only a line can.
    fn next_deadline(&self, context: &CallContext<'_>) -> Option<Timestamp>;
    /// The problem in view now.
    fn problem(&self, context: &CallContext<'_>) -> Option<Problem>;
}

/// Which detector raised a problem. Its code goes in `mc.problem.detector`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DetectorKind {
    MeetCaptions,
    HuddleCaptions,
    Mic,
    RemoteAudio,
    AudioHelper,
    Video,
}

impl DetectorKind {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::MeetCaptions => "meet_captions",
            Self::HuddleCaptions => "huddle_captions",
            Self::Mic => "mic",
            Self::RemoteAudio => "remote_audio",
            Self::AudioHelper => "audio_helper",
            Self::Video => "video",
        }
    }

    /// Whether the detector judges what the helpers record rather than captions.
    #[must_use]
    pub fn is_capture(self) -> bool {
        match self {
            Self::MeetCaptions | Self::HuddleCaptions => false,
            Self::Mic | Self::RemoteAudio | Self::AudioHelper | Self::Video => true,
        }
    }
}
