//! What the tray shows for a meeting, built from its stored step, failure and extraction retry.

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::retry_schedule::EXTRACTION_RETRIES;
use crate::{Meeting, Step};

/// What the tray shows for a meeting.
#[derive(Debug, Clone)]
pub enum JobState {
    Captured,
    Matched,
    Extracted,
    Written {
        has_notes: bool,
    },
    /// Written without notes; an extraction is scheduled. `retry` is `None` for a first try,
    /// queued when a provider was set after the note was written.
    ExtractionPending {
        retry: Option<ExtractionRetry>,
    },
    Failed {
        cause: FailureCause,
    },
}

impl JobState {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            JobState::Captured => "Captured",
            JobState::Matched => "Matched",
            JobState::Extracted => "Extracted",
            JobState::Written { .. } => "Written",
            JobState::ExtractionPending { .. } => "Extraction pending",
            JobState::Failed { .. } => "Failed",
        }
    }
}

/// The next try after a failed extraction. Only `jobs` builds one, so `number` stays within
/// 1 to `EXTRACTION_RETRIES`.
#[derive(Debug, Clone)]
pub struct ExtractionRetry {
    number: u32,
    at: Timestamp,
    last_error: String,
}

impl ExtractionRetry {
    /// This retry's number, from 1 to `EXTRACTION_RETRIES`.
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }

    #[must_use]
    pub fn last_error(&self) -> &str {
        &self.last_error
    }

    /// When this retry runs, as "14:05 IST" in `time_zone`.
    #[must_use]
    pub fn clock_time(&self, time_zone: &TimeZone) -> String {
        self.at
            .to_zoned(time_zone.clone())
            .strftime("%H:%M %Z")
            .to_string()
    }

    /// "retry 2 of 3 at 14:05 IST", with the time in `time_zone`.
    #[must_use]
    pub fn describe(&self, time_zone: &TimeZone) -> String {
        format!(
            "retry {} of {EXTRACTION_RETRIES} at {}",
            self.number,
            self.clock_time(time_zone)
        )
    }
}

/// Why the last attempt failed. A step that next succeeds clears it.
#[derive(Debug, Clone)]
pub struct FailureCause {
    pub kind: FailureKind,
    pub message: String,
}

/// What failed, as stored in `failure_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// A step returned an error or panicked.
    Step,
    /// Extraction failed for good: the last retry failed, or the error would recur on every
    /// retry. The note keeps the transcript only.
    Extraction,
}

impl FailureKind {
    pub(crate) const ALL: [FailureKind; 2] = [FailureKind::Step, FailureKind::Extraction];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FailureKind::Step => "step",
            FailureKind::Extraction => "extraction",
        }
    }

    /// The kind of a failure stored without one. Migration 4 backfills with this rule, and a row
    /// written by a binary older than migration 4 reads with it. An extraction that failed for
    /// good holds the same text in `failure` and `extraction_error`.
    pub(crate) fn inferred(failure: &str, extraction_error: Option<&str>) -> FailureKind {
        if extraction_error == Some(failure) {
            FailureKind::Extraction
        } else {
            FailureKind::Step
        }
    }
}

impl Meeting {
    #[must_use]
    pub fn state(&self) -> JobState {
        if let Some(cause) = &self.failure {
            return JobState::Failed {
                cause: cause.clone(),
            };
        }
        if let Some(at) = self.next_extraction_at {
            // A first try queued by `schedule_missing_extractions` has 0 attempts and can keep the
            // error of an older run. It is no retry.
            let retry = self
                .extraction_error
                .as_ref()
                .filter(|_| self.extraction_attempts >= 1)
                .map(|last_error| ExtractionRetry {
                    number: self.extraction_attempts,
                    at,
                    last_error: last_error.clone(),
                });
            return JobState::ExtractionPending { retry };
        }
        match self.finished_step {
            Step::Captured => JobState::Captured,
            Step::Matched => JobState::Matched,
            Step::Extracted => JobState::Extracted,
            Step::Written => JobState::Written {
                has_notes: self.extraction_json.is_some(),
            },
        }
    }

    /// Whether the next run extracts notes: the step is not done yet, a retry is scheduled, or the
    /// last run failed while notes were still owed, whatever the cause.
    #[must_use]
    pub fn is_extraction_owed(&self) -> bool {
        self.finished_step < Step::Extracted
            || self.next_extraction_at.is_some()
            || (self.failure.is_some() && self.extraction_error.is_some())
    }
}
