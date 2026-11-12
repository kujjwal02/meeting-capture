//! When a failed extraction is tried again, and when the meeting fails instead. When a step that
//! waits on the network is tried again.

use std::time::Duration;

use jiff::Timestamp;

use crate::{Meeting, NetworkFault};

/// How many times a failed extraction is tried again before the meeting fails.
pub(crate) const EXTRACTION_RETRIES: u32 = 3;

// After each failed extraction, the next try waits this long. A failure after the last one fails
// the meeting. Spaced to outlast a rate-limit window or a laptop that went offline.
const EXTRACTION_RETRY_DELAYS: [Duration; EXTRACTION_RETRIES as usize] = [
    Duration::from_mins(5),
    Duration::from_mins(30),
    Duration::from_hours(2),
];

// After each try that fails on the network, the next waits this long. Past the last, every sweep
// tries: a weak uplink can last for days, and each try resumes where the last one stopped.
const NETWORK_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_mins(5),
    Duration::from_mins(30),
    Duration::from_hours(2),
];

// Past the schedule, a service that keeps answering with the same error is broken on its side,
// so each such try doubles the last delay, up to this.
const MAX_SERVICE_FAULT_DELAY: Duration = Duration::from_hours(24);

/// Whether a failed extraction would fail the same way on a retry.
#[derive(Debug, Clone, Copy)]
pub enum ErrorPersistence {
    /// Such as a rate limit or a network error. The schedule tries again.
    Passing,
    /// Such as a missing key or CLI, a refused sign-in or a reply over the output cap. The
    /// meeting fails at once, and the tray says why.
    Lasting,
}

/// What follows a failed extraction.
pub(crate) struct NextExtraction {
    /// Failed attempts so far, this one included.
    pub(crate) attempts: u32,
    /// `None` gives up: the meeting fails.
    pub(crate) retry_at: Option<Timestamp>,
}

/// The next try after `meeting`'s extraction failed in a run that started from `meeting`.
pub(crate) fn after_failure(meeting: &Meeting, persistence: ErrorPersistence) -> NextExtraction {
    // A manual retry after the last failure starts the count again.
    let attempts = match meeting.failure {
        Some(_) => 1,
        None => meeting.extraction_attempts + 1,
    };
    let retry_at = match persistence {
        ErrorPersistence::Lasting => None,
        ErrorPersistence::Passing => retry_delay(attempts).map(|delay| Timestamp::now() + delay),
    };
    NextExtraction { attempts, retry_at }
}

/// What follows a step that failed on the network.
pub(crate) struct NextNetworkTry {
    /// Tries that failed on the network in a row, this one included.
    pub(crate) attempts: u32,
    pub(crate) retry_at: Timestamp,
}

/// The next try after a step of `meeting` failed on the network with `failure`. A try that
/// saved some of its work starts the count again, so a slow link that keeps moving keeps the
/// short delay. Past the schedule, a link fault tries at each sweep, and a service fault that
/// repeats the last failure word for word waits twice as long as the try before.
pub(crate) fn after_network_failure(
    meeting: &Meeting,
    has_progressed: bool,
    fault: NetworkFault,
    failure: &str,
) -> NextNetworkTry {
    let last_wait = meeting
        .failure
        .as_ref()
        .filter(|cause| cause.kind == crate::FailureKind::NetworkWait);
    let attempts = if last_wait.is_some() && !has_progressed {
        meeting.extraction_attempts + 1
    } else {
        1
    };
    let index = usize::try_from(attempts.saturating_sub(1)).unwrap_or(usize::MAX);
    let is_same_failure = last_wait.is_some_and(|cause| cause.message == failure);
    let delay = match (NETWORK_RETRY_DELAYS.get(index), fault) {
        (Some(delay), NetworkFault::Link | NetworkFault::Service) => dev_delay().unwrap_or(*delay),
        (None, NetworkFault::Service) if is_same_failure => {
            let past_schedule = index - NETWORK_RETRY_DELAYS.len() + 1;
            let doublings = u32::try_from(past_schedule).unwrap_or(u32::MAX).min(16);
            let last_delay = NETWORK_RETRY_DELAYS[NETWORK_RETRY_DELAYS.len() - 1];
            let grown = last_delay
                .saturating_mul(2_u32.saturating_pow(doublings))
                .min(MAX_SERVICE_FAULT_DELAY);
            dev_delay().unwrap_or(grown)
        }
        (None, NetworkFault::Link | NetworkFault::Service) => Duration::ZERO,
    };
    NextNetworkTry {
        attempts,
        retry_at: Timestamp::now() + delay,
    }
}

/// Tries in a row before this one that stopped on the same held resource as `failure`. Each
/// such try is stored as a resume with the same message, so a resume of another cause, a success
/// or another failure starts the count again.
pub(crate) fn tries_while_held(meeting: &Meeting, failure: &str) -> u32 {
    meeting
        .failure
        .as_ref()
        .filter(|cause| cause.kind == crate::FailureKind::Resume && cause.message == failure)
        .map_or(0, |_| meeting.extraction_attempts)
}

// `MEETING_CAPTURE_RETRY_DELAY_S` replaces every delay, so a dev run sees the whole schedule in
// minutes.
fn retry_delay(attempts: u32) -> Option<Duration> {
    let index = usize::try_from(attempts).ok()?.checked_sub(1)?;
    let delay = *EXTRACTION_RETRY_DELAYS.get(index)?;
    Some(dev_delay().unwrap_or(delay))
}

fn dev_delay() -> Option<Duration> {
    std::env::var("MEETING_CAPTURE_RETRY_DELAY_S")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map(Duration::from_secs)
}
