//! One row per meeting with a closed state enum; a crash resumes at the last finished step.
//!
//! Step outputs are stored as JSON text. Their types belong to the pipeline, and this crate does
//! not read them.
//!
//! The tray app and `mc` are separate processes on one database. A run holds a lease on its
//! meeting, so only one run per meeting is active. `JobStore::run` holds the lease while its
//! steps run, and each step updates the meeting through the `LeasedRun` it is given. Every step
//! update requires the lease and renews it. A new capture of the same meeting bumps its input
//! revision; step updates from a run on an older revision change nothing and report
//! `StoreError::Superseded`, and the run starts over on the new input.

mod lease;
mod retry_schedule;
mod schema;
mod state;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use rusqlite_migration::Migrations;

pub use crate::lease::{LeasedRun, RunOutcome, StepError};
pub use crate::retry_schedule::ErrorPersistence;
use crate::schema::{MEETING_COLUMNS, MIGRATIONS, read_meeting};
pub use crate::state::{ExtractionRetry, FailureCause, FailureKind, JobState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MeetingId(i64);

impl MeetingId {
    #[must_use]
    pub fn new(id: i64) -> Self {
        Self(id)
    }

    #[must_use]
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for MeetingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The last pipeline step a meeting finished. A retry resumes after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Captured,
    Matched,
    Extracted,
    Written,
}

impl Step {
    const ALL: [Step; 4] = [
        Step::Captured,
        Step::Matched,
        Step::Extracted,
        Step::Written,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Step::Captured => "captured",
            Step::Matched => "matched",
            Step::Extracted => "extracted",
            Step::Written => "written",
        }
    }
}

/// The stored input a meeting's transcript is parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// A raw TranscripTonic advanced webhook body.
    TranscriptonicPayload,
    /// A Slack huddle log written by the huddle watcher, one JSON line per event.
    SlackHuddleLog,
}

impl InputKind {
    const ALL: [InputKind; 2] = [InputKind::TranscriptonicPayload, InputKind::SlackHuddleLog];

    fn as_str(self) -> &'static str {
        match self {
            InputKind::TranscriptonicPayload => "transcriptonic-payload",
            InputKind::SlackHuddleLog => "slack-huddle-log",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Meeting {
    pub id: MeetingId,
    pub input_kind: InputKind,
    pub input_path: PathBuf,
    /// Bumped by every new capture of this meeting and by a rewind.
    pub input_revision: i64,
    pub started_at: Timestamp,
    /// The calendar event title once matched, else the title the call app showed.
    pub title: Option<String>,
    pub finished_step: Step,
    // Set when the last attempt failed. Cleared when a step next succeeds.
    failure: Option<FailureCause>,
    pub retry_count: u32,
    pub calendar_match_json: Option<String>,
    pub extraction_json: Option<String>,
    /// `<provider>/<model>` of the last extraction tried.
    pub extraction_provider: Option<String>,
    // Failed extraction attempts since the last success or manual retry.
    extraction_attempts: u32,
    // The last extraction failure. The note was written without notes.
    extraction_error: Option<String>,
    // When set, an extraction is owed and the sweep runs it from this time.
    next_extraction_at: Option<Timestamp>,
    /// The note in the vault and the hashes that tell the pipeline's writes from the user's
    /// edits.
    pub note: NoteOwnership,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The meeting's note in the vault, and what the pipeline knows it wrote there.
#[derive(Debug, Clone)]
pub struct NoteOwnership {
    // `None` until a run claims a path. A run claims the path before its first write, so no
    // hash exists without one.
    claimed: Option<ClaimedNote>,
}

#[derive(Debug, Clone)]
struct ClaimedNote {
    path: PathBuf,
    written_sha256: Option<String>,
    // Each note write that started but was not recorded, after a crash or a re-post mid-write.
    pending_sha256s: Vec<String>,
    section_sha256s: BTreeMap<String, String>,
}

impl NoteOwnership {
    /// Chosen before the first write, so a rerun after a crash rewrites the same note.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.claimed.as_ref().map(|claimed| claimed.path.as_path())
    }

    /// SHA-256 of every note the pipeline wrote or started to write. The pipeline still owns a
    /// note with any of these contents; other contents mean the user edited it. Empty for a new
    /// note, or after the user chose to discard their edits.
    #[must_use]
    pub fn known_sha256s(&self) -> Vec<&str> {
        self.claimed
            .iter()
            .flat_map(|claimed| {
                claimed
                    .written_sha256
                    .iter()
                    .chain(&claimed.pending_sha256s)
            })
            .map(String::as_str)
            .collect()
    }

    /// SHA-256 of each marked note section as last written, by section name. A section whose
    /// contents differ was edited by the user, and a rerun that would replace it refuses
    /// instead.
    #[must_use]
    pub fn section_sha256s(&self) -> &BTreeMap<String, String> {
        static NO_SECTIONS: BTreeMap<String, String> = BTreeMap::new();
        self.claimed
            .as_ref()
            .map_or(&NO_SECTIONS, |claimed| &claimed.section_sha256s)
    }
}

/// A successful extraction, as stored.
#[derive(Debug, Clone, Copy)]
pub struct Extraction<'a> {
    pub notes_json: &'a str,
    /// `<provider>/<model>`.
    pub provider: &'a str,
}

/// How the extraction of a run that writes the note ended, as `LeasedRun::mark_written` records
/// it.
#[derive(Debug, Clone, Copy)]
pub enum ExtractionAtWrite<'a> {
    /// The run stored its notes, or that no provider is set, before the write. Or the run owed
    /// no extraction.
    Recorded,
    /// The note is written without notes.
    Failed(ExtractionFailure<'a>),
}

/// A failed extraction, as recorded with the note written without notes.
#[derive(Debug, Clone, Copy)]
pub struct ExtractionFailure<'a> {
    /// `<provider>/<model>`, or the provider alone when it could not be set up.
    pub provider: &'a str,
    pub error: &'a str,
    pub persistence: ErrorPersistence,
}

/// What a note path claim found.
#[derive(Debug, PartialEq, Eq)]
pub enum NotePathClaim {
    Claimed,
    /// Another meeting holds this path.
    TakenByOtherMeeting,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("job store query failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("job store migration failed: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    #[error("could not lock {path} to open the job store: {source}")]
    Lock {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("meeting {0} is not in the job store")]
    NotFound(MeetingId),
    #[error("meeting {0} was captured again while this run was in progress")]
    Superseded(MeetingId),
    #[error("meeting {0}: this run's lease expired and another run took the meeting")]
    LeaseLost(MeetingId),
    #[error("meeting {0} already has a note path; a second run must not choose another")]
    NotePathAlreadySet(MeetingId),
    #[error("meeting {id} has an invalid {column} {value:?}; valid values: {valid}")]
    InvalidColumn {
        id: i64,
        column: &'static str,
        value: String,
        valid: String,
    },
}

const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// The job store. Safe to share between threads; each call holds the connection briefly.
pub struct JobStore {
    connection: Mutex<Connection>,
}

impl JobStore {
    /// Opens the database at `path`, creating it and applying migrations as needed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the file cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        // The tray app and `mc` share this file and may open it at the same moment. Switching to
        // WAL skips SQLite's busy handler, and the migration reads the schema version outside
        // its transaction, so one process at a time does both.
        let lock_path = path.with_extension("sqlite.lock");
        let open_lock = std::fs::File::create(&lock_path).map_err(|source| StoreError::Lock {
            path: lock_path.clone(),
            source,
        })?;
        open_lock.lock().map_err(|source| StoreError::Lock {
            path: lock_path,
            source,
        })?;

        let mut connection = Connection::open(path)?;
        // A deferred transaction that later writes fails at once with "database is locked"
        // instead of waiting, so transactions take the write lock up front.
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.set_transaction_behavior(TransactionBehavior::Immediate);
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Migrations::from_slice(MIGRATIONS).to_latest(&mut connection)?;
        drop(open_lock);
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Records a captured meeting whose input is saved at `input_path`.
    ///
    /// A meeting is identified by its input kind and start time. A second capture of the same
    /// meeting, such as a TranscripTonic re-post, replaces the stored input, bumps the input
    /// revision and sends the meeting back to `Captured`, so its next run rewrites the same note.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the write fails.
    pub fn upsert_captured(
        &self,
        input_kind: InputKind,
        input_path: &Path,
        started_at: Timestamp,
        title: Option<&str>,
    ) -> Result<MeetingId, StoreError> {
        let id = self.connection().query_row(
            "INSERT INTO meetings
                (input_kind, input_path, started_at, title, finished_step, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT (input_kind, started_at) DO UPDATE SET
                input_path = excluded.input_path,
                input_revision = input_revision + 1,
                title = excluded.title,
                finished_step = excluded.finished_step,
                failure = NULL, failure_kind = NULL,
                calendar_match_json = NULL,
                extraction_json = NULL,
                extraction_provider = NULL,
                extraction_attempts = 0,
                extraction_error = NULL,
                next_extraction_at_ms = NULL,
                updated_at = excluded.updated_at
             RETURNING id",
            params![
                input_kind.as_str(),
                input_path.to_string_lossy(),
                started_at.to_string(),
                title,
                Step::Captured.as_str(),
                Timestamp::now().to_string(),
            ],
            |row| row.get(0),
        )?;
        Ok(MeetingId(id))
    }

    /// # Errors
    ///
    /// Returns `StoreError::NotFound` for an unknown id.
    pub fn get(&self, id: MeetingId) -> Result<Meeting, StoreError> {
        self.connection()
            .query_row(
                &format!("SELECT {MEETING_COLUMNS} FROM meetings WHERE id = ?1"),
                [id.0],
                read_meeting,
            )
            .optional()?
            .ok_or(StoreError::NotFound(id))?
    }

    /// The `limit` most recent meetings by start time, newest first.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the query fails or a row is invalid.
    pub fn recent(&self, limit: u32) -> Result<Vec<Meeting>, StoreError> {
        let connection = self.connection();
        let mut statement = connection.prepare(&format!(
            "SELECT {MEETING_COLUMNS} FROM meetings ORDER BY started_at DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map([limit], read_meeting)?;
        rows.map(|row| row?).collect()
    }

    /// Whether a meeting was recorded from the input at `input_path`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the query fails.
    pub fn has_input(&self, input_path: &Path) -> Result<bool, StoreError> {
        Ok(self.connection().query_row(
            "SELECT EXISTS (SELECT 1 FROM meetings WHERE input_path = ?1)",
            [input_path.to_string_lossy()],
            |row| row.get(0),
        )?)
    }

    /// Meetings to run again, oldest first: those that stopped before their note was written
    /// without a recorded failure (the app quit or crashed mid-run, or a capture arrived while it
    /// was down), and written ones whose extraction retry is due.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the query fails or a row is invalid.
    pub fn unfinished(&self) -> Result<Vec<Meeting>, StoreError> {
        let connection = self.connection();
        let mut statement = connection.prepare(&format!(
            "SELECT {MEETING_COLUMNS} FROM meetings
             WHERE failure IS NULL
                AND (finished_step != ?1 OR next_extraction_at_ms <= ?2)
             ORDER BY started_at"
        ))?;
        let rows = statement.query_map(
            params![Step::Written.as_str(), Timestamp::now().as_millisecond()],
            read_meeting,
        )?;
        rows.map(|row| row?).collect()
    }

    /// Meetings whose last run failed, oldest first.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the query fails or a row is invalid.
    pub fn failed(&self) -> Result<Vec<Meeting>, StoreError> {
        let connection = self.connection();
        let mut statement = connection.prepare(&format!(
            "SELECT {MEETING_COLUMNS} FROM meetings WHERE failure IS NOT NULL ORDER BY started_at"
        ))?;
        let rows = statement.query_map([], read_meeting)?;
        rows.map(|row| row?).collect()
    }

    /// Sends a meeting back to `Captured` so every step runs again. Keeps the note path. With
    /// `discard_note_edits`, the next write overwrites a note the user edited.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` for an unknown id.
    pub fn rewind(&self, id: MeetingId, discard_note_edits: bool) -> Result<(), StoreError> {
        let updated = self.connection().execute(
            "UPDATE meetings SET finished_step = ?2, input_revision = input_revision + 1,
                failure = NULL, failure_kind = NULL, calendar_match_json = NULL,
                extraction_json = NULL,
                extraction_provider = NULL, extraction_attempts = 0, extraction_error = NULL,
                next_extraction_at_ms = NULL,
                note_sha256 = CASE WHEN ?3 THEN NULL ELSE note_sha256 END,
                pending_note_sha256s = CASE WHEN ?3 THEN NULL ELSE pending_note_sha256s END,
                note_section_sha256s = CASE WHEN ?3 THEN NULL ELSE note_section_sha256s END,
                updated_at = ?4
             WHERE id = ?1",
            params![
                id.0,
                Step::Captured.as_str(),
                discard_note_edits,
                Timestamp::now().to_string()
            ],
        )?;
        match updated {
            0 => Err(StoreError::NotFound(id)),
            _ => Ok(()),
        }
    }

    /// Joins two records of one call: deletes `dropped`, then gives `kept` the merged input and
    /// start time and sends it back to `Captured`, as `rewind` does. One transaction, so the
    /// start time `dropped` held is free for `kept`. Returns the note path `dropped` held, which
    /// the caller removes.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` when either meeting is unknown.
    pub fn merge(
        &self,
        kept: MeetingId,
        dropped: MeetingId,
        input_path: &Path,
        started_at: Timestamp,
    ) -> Result<Option<PathBuf>, StoreError> {
        let mut connection = self.connection();
        let transaction = connection.transaction()?;
        let dropped_note: Option<String> = transaction
            .query_row(
                "DELETE FROM meetings WHERE id = ?1 RETURNING note_path",
                [dropped.0],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound(dropped))?;
        let updated = transaction.execute(
            "UPDATE meetings SET input_path = ?2, started_at = ?3, finished_step = ?4,
                input_revision = input_revision + 1,
                failure = NULL, failure_kind = NULL, calendar_match_json = NULL,
                extraction_json = NULL,
                extraction_provider = NULL, extraction_attempts = 0, extraction_error = NULL,
                next_extraction_at_ms = NULL, updated_at = ?5
             WHERE id = ?1",
            params![
                kept.0,
                input_path.to_string_lossy(),
                started_at.to_string(),
                Step::Captured.as_str(),
                Timestamp::now().to_string()
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::NotFound(kept));
        }
        transaction.commit()?;
        Ok(dropped_note.map(PathBuf::from))
    }

    /// Schedules an extraction now for every written meeting that has none and no failure, such
    /// as notes written while no provider was set. Returns how many were scheduled.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the write fails.
    pub fn schedule_missing_extractions(&self) -> Result<usize, StoreError> {
        let now = Timestamp::now();
        Ok(self.connection().execute(
            "UPDATE meetings SET next_extraction_at_ms = ?2, extraction_attempts = 0,
                updated_at = ?3
             WHERE finished_step = ?1 AND extraction_json IS NULL AND failure IS NULL
                AND next_extraction_at_ms IS NULL",
            params![
                Step::Written.as_str(),
                now.as_millisecond(),
                now.to_string()
            ],
        )?)
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        // A panic mid-query leaves no half-applied state: each call is one SQLite statement.
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

// The note half of `LeasedRun::mark_written`, shared so the note records the same way whatever
// the extraction outcome.
const NOTE_WRITTEN: &str = "finished_step = ?6, note_sha256 = ?7, pending_note_sha256s = NULL,
    note_section_sha256s = ?8";

impl LeasedRun<'_> {
    /// `title` is the calendar event title, else the title the call app showed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn record_match(
        &self,
        title: Option<&str>,
        calendar_match_json: &str,
    ) -> Result<(), StoreError> {
        self.update(
            "finished_step = ?6, title = ?7, calendar_match_json = ?8, failure = NULL,
                failure_kind = NULL",
            &[&Step::Matched.as_str(), &title, &calendar_match_json],
        )
    }

    /// Records the extraction step as done. With `extraction` as `None`, no provider is set and
    /// the note carries only the transcript; nothing is retried.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn record_extraction(&self, extraction: Option<Extraction<'_>>) -> Result<(), StoreError> {
        let (extraction_json, provider) = match extraction {
            Some(extraction) => (Some(extraction.notes_json), Some(extraction.provider)),
            None => (None, None),
        };
        self.update(
            "finished_step = ?6, extraction_json = ?7, extraction_provider = ?8,
                extraction_attempts = 0, extraction_error = NULL, next_extraction_at_ms = NULL,
                failure = NULL, failure_kind = NULL",
            &[&Step::Extracted.as_str(), &extraction_json, &provider],
        )
    }

    /// Gives `note_path` to the meeting. The unique index makes the check and the claim one step,
    /// so two meetings never share a note.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotePathAlreadySet` when the meeting already has a path, and
    /// `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn claim_note_path(&self, note_path: &Path) -> Result<NotePathClaim, StoreError> {
        let note_path = note_path.to_string_lossy();
        let claim = self.update(
            "note_path = CASE WHEN note_path IS NULL THEN ?6 ELSE note_path END",
            &[&note_path],
        );
        match claim {
            Ok(()) => {}
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)))
                if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                return Ok(NotePathClaim::TakenByOtherMeeting);
            }
            Err(error) => return Err(error),
        }
        let stored: Option<String> = self.store.connection().query_row(
            "SELECT note_path FROM meetings WHERE id = ?1",
            [self.meeting().id.0],
            |row| row.get(0),
        )?;
        match stored {
            Some(stored) if stored == note_path => Ok(NotePathClaim::Claimed),
            Some(_) | None => Err(StoreError::NotePathAlreadySet(self.meeting().id)),
        }
    }

    /// Adds the hash of a note about to be written, before the write. Earlier pending hashes stay
    /// until `mark_written`, since any of them may be the note on disk.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn record_pending_note(&self, note_sha256: &str) -> Result<(), StoreError> {
        self.update(
            "pending_note_sha256s = CASE WHEN pending_note_sha256s IS NULL THEN ?6
                ELSE pending_note_sha256s || ' ' || ?6 END",
            &[&note_sha256],
        )
    }

    /// Records the note as written and, when it failed, the run's extraction, in one statement.
    /// A crash before it leaves the step and the extraction as they were before the write, plus
    /// the pending note hash, so the sweep or a manual retry runs the meeting again. After a failed extraction the retry schedule sets the next try, and the
    /// sweep runs it. After the last retry, or at once for a lasting error, the meeting fails with
    /// the error.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn mark_written(
        &self,
        note_sha256: &str,
        section_sha256s: &BTreeMap<String, String>,
        extraction: ExtractionAtWrite<'_>,
    ) -> Result<(), StoreError> {
        let section_lines = section_sha256s
            .iter()
            .map(|(section, sha256)| format!("{section} {sha256}"))
            .collect::<Vec<_>>()
            .join("\n");
        let written = Step::Written.as_str();
        match extraction {
            ExtractionAtWrite::Recorded => self.update(
                &format!("{NOTE_WRITTEN}, failure = NULL, failure_kind = NULL"),
                &[&written, &note_sha256, &section_lines],
            ),
            ExtractionAtWrite::Failed(ExtractionFailure {
                provider,
                error,
                persistence,
            }) => {
                let next = retry_schedule::after_failure(self.meeting(), persistence);
                let retry_at_ms = next.retry_at.map(Timestamp::as_millisecond);
                self.update(
                    &format!(
                        "{NOTE_WRITTEN}, extraction_provider = ?9, extraction_attempts = ?10,
                            extraction_error = ?11, next_extraction_at_ms = ?12,
                            failure = CASE WHEN ?12 IS NULL THEN ?11 ELSE NULL END,
                            failure_kind = CASE WHEN ?12 IS NULL THEN ?13 ELSE NULL END"
                    ),
                    &[
                        &written,
                        &note_sha256,
                        &section_lines,
                        &provider,
                        &next.attempts,
                        &error,
                        &retry_at_ms,
                        &FailureKind::Extraction.as_str(),
                    ],
                )
            }
        }
    }
}
