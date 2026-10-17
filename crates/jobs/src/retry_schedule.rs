//! When a failed extraction is tried again, and when the meeting fails instead.

use std::time::Duration;

use jiff::Timestamp;

use crate::Meeting;

/// How many times a failed extraction is tried again before the meeting fails.
pub(crate) const EXTRACTION_RETRIES: u32 = 3;

// After each failed extraction, the next try waits this long. A failure after the last one fails
// the meeting. Spaced to outlast a rate-limit window or a laptop that went offline.
const EXTRACTION_RETRY_DELAYS: [Duration; EXTRACTION_RETRIES as usize] = [
    Duration::from_mins(5),
    Duration::from_mins(30),
    Duration::from_hours(2),
];

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

// `MEETING_CAPTURE_RETRY_DELAY_S` replaces every delay, so a dev run sees the whole schedule in
// minutes.
fn retry_delay(attempts: u32) -> Option<Duration> {
    let index = usize::try_from(attempts).ok()?.checked_sub(1)?;
    let delay = *EXTRACTION_RETRY_DELAYS.get(index)?;
    let dev_delay = std::env::var("MEETING_CAPTURE_RETRY_DELAY_S")
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map(Duration::from_secs);
    Some(dev_delay.unwrap_or(delay))
}
