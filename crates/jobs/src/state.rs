//! What the tray shows for a meeting, built from its stored step, failure and extraction retry.

use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::retry_schedule::EXTRACTION_RETRIES;
use crate::{Meeting, Step};

/// What the tray shows for a meeting.
#[derive(Debug, Clone)]
pub enum JobState {
    Captured,
    Transcribed,
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
    /// A step could not reach a service, such as speech-to-text on a weak uplink. The sweep
    /// tries again on a schedule; nothing is wrong with the meeting.
    NetworkWait {
        retry: NetworkRetry,
    },
    /// A step saved part of its work and ran out of run time. The next sweep resumes it with no
    /// delay; nothing is wrong with the meeting.
    Resuming {
        /// What the step said when it stopped.
        message: String,
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
            JobState::Transcribed => "Transcribed",
            JobState::Matched => "Matched",
            JobState::Extracted => "Extracted",
            JobState::Written { .. } => "Written",
            JobState::ExtractionPending { .. } => "Extraction pending",
            JobState::NetworkWait { .. } => "Waiting for the network",
            JobState::Resuming { .. } => "Resuming",
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

/// The next try of a step that failed on the network. Only `jobs` builds one.
#[derive(Debug, Clone)]
pub struct NetworkRetry {
    number: u32,
    at: Timestamp,
    last_error: String,
}

impl NetworkRetry {
    #[must_use]
    pub fn number(&self) -> u32 {
        self.number
    }

    #[must_use]
    pub fn last_error(&self) -> &str {
        &self.last_error
    }

    /// "retry 2 at 14:05 IST" with the time in `time_zone`, the weekday too when it is not
    /// today, or "retry 4 at the next sweep" when the try is due already.
    #[must_use]
    pub fn describe(&self, time_zone: &TimeZone) -> String {
        let now = Timestamp::now();
        if self.at <= now {
            return format!("retry {} at the next sweep", self.number);
        }
        let at = self.at.to_zoned(time_zone.clone());
        let format = if at.date() == now.to_zoned(time_zone.clone()).date() {
            "%H:%M %Z"
        } else {
            "%a %H:%M %Z"
        };
        format!("retry {} at {}", self.number, at.strftime(format))
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
    /// A step could not reach a service. The schedule in `next_extraction_at_ms` runs it again,
    /// and `extraction_attempts` counts the tries, since a waiting meeting owes its extraction
    /// too.
    NetworkWait,
    /// A step saved part of its work and ran out of run time. `next_extraction_at_ms` holds the
    /// time it stopped, so the next sweep runs it.
    Resume,
}

impl FailureKind {
    pub(crate) const ALL: [FailureKind; 4] = [
        FailureKind::Step,
        FailureKind::Extraction,
        FailureKind::NetworkWait,
        FailureKind::Resume,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            FailureKind::Step => "step",
            FailureKind::Extraction => "extraction",
            FailureKind::NetworkWait => "network-wait",
            FailureKind::Resume => "resume",
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
            match cause.kind {
                FailureKind::NetworkWait => {
                    return JobState::NetworkWait {
                        retry: NetworkRetry {
                            number: self.extraction_attempts.max(1),
                            at: self.next_extraction_at.unwrap_or(self.updated_at),
                            last_error: cause.message.clone(),
                        },
                    };
                }
                FailureKind::Resume => {
                    return JobState::Resuming {
                        message: cause.message.clone(),
                    };
                }
                FailureKind::Step | FailureKind::Extraction => {}
            }
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
            Step::Transcribed => JobState::Transcribed,
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

    /// Whether the meeting, not failed, has a run due now: a pending transcript or a retry
    /// whose time has come.
    #[must_use]
    pub fn is_run_due(&self) -> bool {
        self.failure.is_none()
            && self
                .next_extraction_at
                .is_some_and(|at| at <= Timestamp::now())
    }
}
