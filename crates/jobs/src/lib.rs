//! One row per meeting with a closed state enum; a crash resumes at the last finished step.
//!
//! Step outputs are stored as JSON text. Their types belong to the pipeline, and this crate does
//! not read them.
//!
//! The tray app and `mc` are separate processes on one database. A run holds a lease on its
//! meeting, so only one run per meeting is active. Every step update requires the lease and
//! renews it. A new capture of the same meeting bumps its input revision; step updates from a run
//! on an older revision change nothing and report `StoreError::Superseded`, and the run starts
//! over on the new input.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Row, ToSql, TransactionBehavior, params};
use rusqlite_migration::{M, Migrations};

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

/// What the tray shows for a meeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Captured,
    Matched,
    Extracted,
    Written,
    /// Written without notes; an extraction retry is scheduled.
    ExtractionPending,
    Failed,
}

impl JobState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            JobState::Captured => "Captured",
            JobState::Matched => "Matched",
            JobState::Extracted => "Extracted",
            JobState::Written => "Written",
            JobState::ExtractionPending => "Extraction pending",
            JobState::Failed => "Failed",
        }
    }
}

/// The stored input a meeting's transcript is parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    /// A raw TranscripTonic advanced webhook body.
    TranscriptonicPayload,
}

impl InputKind {
    const ALL: [InputKind; 1] = [InputKind::TranscriptonicPayload];

    fn as_str(self) -> &'static str {
        match self {
            InputKind::TranscriptonicPayload => "transcriptonic-payload",
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
    /// Set when the last attempt failed; names the step and the cause. Cleared when a step
    /// next succeeds.
    pub failure: Option<String>,
    pub retry_count: u32,
    pub calendar_match_json: Option<String>,
    pub extraction_json: Option<String>,
    /// `<provider>/<model>` of the last extraction tried.
    pub extraction_provider: Option<String>,
    /// Failed extraction attempts since the last success or manual retry.
    pub extraction_attempts: u32,
    /// The last extraction failure. The note was written without notes.
    pub extraction_error: Option<String>,
    /// When set, an extraction is owed and the sweep runs it from this time.
    pub next_extraction_at: Option<Timestamp>,
    /// Chosen before the first write, so a rerun after a crash rewrites the same note.
    pub note_path: Option<PathBuf>,
    /// SHA-256 of the note as the pipeline last wrote it. Other contents on disk mean the user
    /// edited the note.
    pub note_sha256: Option<String>,
    /// SHA-256 of each note write that started but was not recorded, after a crash or a re-post
    /// mid-write. The pipeline still owns a note with any of these contents.
    pub pending_note_sha256s: Vec<String>,
    /// SHA-256 of each marked note section as last written, by section name. A section whose
    /// contents differ was edited by the user, and a rerun leaves it alone.
    pub note_section_sha256s: BTreeMap<String, String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Meeting {
    #[must_use]
    pub fn state(&self) -> JobState {
        if self.failure.is_some() {
            return JobState::Failed;
        }
        if self.next_extraction_at.is_some() {
            return JobState::ExtractionPending;
        }
        match self.finished_step {
            Step::Captured => JobState::Captured,
            Step::Matched => JobState::Matched,
            Step::Extracted => JobState::Extracted,
            Step::Written => JobState::Written,
        }
    }
}

/// A successful extraction, as stored.
#[derive(Debug, Clone, Copy)]
pub struct Extraction<'a> {
    pub notes_json: &'a str,
    /// `<provider>/<model>`.
    pub provider: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct ExtractionFailure<'a> {
    /// `<provider>/<model>`, or the provider alone when it could not be set up.
    pub provider: &'a str,
    pub error: &'a str,
    /// Failed attempts so far, this one included.
    pub attempts: u32,
    /// `None` gives up: the meeting fails.
    pub retry_at: Option<Timestamp>,
}

/// Proof that this process runs the pipeline for one meeting. A clone lets a second thread keep
/// the lease alive.
#[derive(Debug, Clone)]
pub struct RunLease {
    id: MeetingId,
    token: String,
}

impl RunLease {
    #[must_use]
    pub fn meeting_id(&self) -> MeetingId {
        self.id
    }
}

/// How giving back a lease went.
#[derive(Debug, PartialEq, Eq)]
pub enum LeaseRelease {
    Released,
    /// A new capture arrived during the run. The lease is still held; run again.
    Superseded,
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

/// A lease not renewed for this long belongs to a process that died mid-run. Every step update
/// renews it, and a running pipeline calls `JobStore::renew_lease` well within this time.
pub const LEASE_EXPIRY: Duration = Duration::from_mins(2);

#[expect(clippy::cast_possible_truncation, reason = "two minutes fits in i64")]
const LEASE_EXPIRY_MS: i64 = LEASE_EXPIRY.as_millis() as i64;

const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

const MIGRATIONS: &[M<'static>] = &[
    M::up(
        "CREATE TABLE meetings (
        id INTEGER PRIMARY KEY,
        input_kind TEXT NOT NULL,
        input_path TEXT NOT NULL,
        input_revision INTEGER NOT NULL DEFAULT 1,
        started_at TEXT NOT NULL,
        title TEXT,
        finished_step TEXT NOT NULL,
        failure TEXT,
        retry_count INTEGER NOT NULL DEFAULT 0,
        calendar_match_json TEXT,
        extraction_json TEXT,
        note_path TEXT,
        note_sha256 TEXT,
        pending_note_sha256s TEXT,
        run_token TEXT,
        run_renewed_at_ms INTEGER,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE UNIQUE INDEX meetings_input_started_at ON meetings (input_kind, started_at);
    CREATE INDEX meetings_started_at ON meetings (started_at);
    CREATE UNIQUE INDEX meetings_note_path ON meetings (note_path);",
    ),
    // Existing rows read as "no extraction tried yet". Times are UTC milliseconds, which sort as
    // numbers; RFC 3339 text with varying fractions does not.
    M::up(
        "ALTER TABLE meetings ADD COLUMN extraction_provider TEXT;
    ALTER TABLE meetings ADD COLUMN extraction_attempts INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE meetings ADD COLUMN extraction_error TEXT;
    ALTER TABLE meetings ADD COLUMN next_extraction_at_ms INTEGER;",
    ),
    // One "<section> <sha256>" line per marked section of the note.
    M::up("ALTER TABLE meetings ADD COLUMN note_section_sha256s TEXT;"),
];

const MEETING_COLUMNS: &str = "id, input_kind, input_path, input_revision, started_at, title, \
    finished_step, failure, retry_count, calendar_match_json, extraction_json, \
    extraction_provider, extraction_attempts, extraction_error, next_extraction_at_ms, note_path, \
    note_sha256, pending_note_sha256s, note_section_sha256s, created_at, updated_at";

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
                failure = NULL,
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

    /// Takes the run lease for a meeting. Returns `None` while another run holds it. An attempt
    /// after a failure counts as a retry; the failure stays visible until a step succeeds.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotFound` for an unknown id.
    pub fn lease_run(&self, id: MeetingId) -> Result<Option<RunLease>, StoreError> {
        static LEASE_COUNTER: AtomicU64 = AtomicU64::new(0);
        let now_ms = Timestamp::now().as_millisecond();
        let token = format!(
            "{}-{now_ms}-{}",
            std::process::id(),
            LEASE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let leased = self.connection().execute(
            "UPDATE meetings SET run_token = ?2, run_renewed_at_ms = ?3,
                retry_count = retry_count + (failure IS NOT NULL)
             WHERE id = ?1 AND (run_token IS NULL OR run_renewed_at_ms < ?4)",
            params![id.0, token, now_ms, now_ms - LEASE_EXPIRY_MS],
        )?;
        if leased == 0 {
            self.get(id)?;
            return Ok(None);
        }
        Ok(Some(RunLease { id, token }))
    }

    /// Keeps the lease alive during a long step.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::LeaseLost` when another run took the meeting.
    pub fn renew_lease(&self, lease: &RunLease) -> Result<(), StoreError> {
        let renewed = self.connection().execute(
            "UPDATE meetings SET run_renewed_at_ms = ?3 WHERE id = ?1 AND run_token = ?2",
            params![lease.id.0, lease.token, Timestamp::now().as_millisecond()],
        )?;
        match renewed {
            0 => Err(StoreError::LeaseLost(lease.id)),
            _ => Ok(()),
        }
    }

    /// Gives the lease back if the meeting is still on `input_revision`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::LeaseLost` when another run took the meeting.
    pub fn release_run(
        &self,
        lease: &RunLease,
        input_revision: i64,
    ) -> Result<LeaseRelease, StoreError> {
        self.release_leased(lease, input_revision, "failure = failure", &[])
    }

    /// Records a failed run and gives the lease back, in one step, so a capture that arrives in
    /// between is never left without a run.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::LeaseLost` when another run took the meeting.
    pub fn fail_run(
        &self,
        lease: &RunLease,
        input_revision: i64,
        failure: &str,
    ) -> Result<LeaseRelease, StoreError> {
        self.release_leased(lease, input_revision, "failure = ?6", &[&failure])
    }

    /// Gives the lease back whatever the revision, when a run stops on a store error or a panic.
    ///
    /// # Errors
    ///
    /// Returns `StoreError` when the write fails.
    pub fn abandon_run(&self, lease: &RunLease) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE meetings SET run_token = NULL, run_renewed_at_ms = NULL
             WHERE id = ?1 AND run_token = ?2",
            params![lease.id.0, lease.token],
        )?;
        Ok(())
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
                failure = NULL, calendar_match_json = NULL, extraction_json = NULL,
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

    /// `title` is the calendar event title, else the title the call app showed.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn record_match(
        &self,
        lease: &RunLease,
        input_revision: i64,
        title: Option<&str>,
        calendar_match_json: &str,
    ) -> Result<(), StoreError> {
        self.update_leased(
            lease,
            input_revision,
            "finished_step = ?6, title = ?7, calendar_match_json = ?8, failure = NULL",
            &[&Step::Matched.as_str(), &title, &calendar_match_json],
        )
    }

    /// Records the extraction step as done. With `extraction` as `None`, no provider is set and
    /// the note carries only the transcript; nothing is retried.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn record_extraction(
        &self,
        lease: &RunLease,
        input_revision: i64,
        extraction: Option<Extraction<'_>>,
    ) -> Result<(), StoreError> {
        let (extraction_json, provider) = match extraction {
            Some(extraction) => (Some(extraction.notes_json), Some(extraction.provider)),
            None => (None, None),
        };
        self.update_leased(
            lease,
            input_revision,
            "finished_step = ?6, extraction_json = ?7, extraction_provider = ?8,
                extraction_attempts = 0, extraction_error = NULL, next_extraction_at_ms = NULL,
                failure = NULL",
            &[&Step::Extracted.as_str(), &extraction_json, &provider],
        )
    }

    /// Records a failed extraction once the note is written without notes. With `retry_at`, the
    /// sweep tries again from then; without, the meeting fails with `error`.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn record_extraction_failure(
        &self,
        lease: &RunLease,
        input_revision: i64,
        failure: ExtractionFailure<'_>,
    ) -> Result<(), StoreError> {
        let retry_at_ms = failure.retry_at.map(Timestamp::as_millisecond);
        self.update_leased(
            lease,
            input_revision,
            "extraction_provider = ?6, extraction_attempts = ?7, extraction_error = ?8,
                next_extraction_at_ms = ?9,
                failure = CASE WHEN ?9 IS NULL THEN ?8 ELSE NULL END",
            &[
                &failure.provider,
                &failure.attempts,
                &failure.error,
                &retry_at_ms,
            ],
        )
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

    /// Gives `note_path` to the meeting. The unique index makes the check and the claim one step,
    /// so two meetings never share a note.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::NotePathAlreadySet` when the meeting already has a path, and
    /// `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn claim_note_path(
        &self,
        lease: &RunLease,
        input_revision: i64,
        note_path: &Path,
    ) -> Result<NotePathClaim, StoreError> {
        let note_path = note_path.to_string_lossy();
        let claim = self.update_leased(
            lease,
            input_revision,
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
        let stored: Option<String> = self.connection().query_row(
            "SELECT note_path FROM meetings WHERE id = ?1",
            [lease.id.0],
            |row| row.get(0),
        )?;
        match stored {
            Some(stored) if stored == note_path => Ok(NotePathClaim::Claimed),
            Some(_) | None => Err(StoreError::NotePathAlreadySet(lease.id)),
        }
    }

    /// Adds the hash of a note about to be written, before the write. Earlier pending hashes stay
    /// until `mark_written`, since any of them may be the note on disk.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn record_pending_note(
        &self,
        lease: &RunLease,
        input_revision: i64,
        note_sha256: &str,
    ) -> Result<(), StoreError> {
        self.update_leased(
            lease,
            input_revision,
            "pending_note_sha256s = CASE WHEN pending_note_sha256s IS NULL THEN ?6
                ELSE pending_note_sha256s || ' ' || ?6 END",
            &[&note_sha256],
        )
    }

    /// # Errors
    ///
    /// Returns `StoreError::Superseded` or `StoreError::LeaseLost` when the run must stop.
    pub fn mark_written(
        &self,
        lease: &RunLease,
        input_revision: i64,
        note_sha256: &str,
        section_sha256s: &BTreeMap<String, String>,
    ) -> Result<(), StoreError> {
        let section_lines: Vec<String> = section_sha256s
            .iter()
            .map(|(section, sha256)| format!("{section} {sha256}"))
            .collect();
        self.update_leased(
            lease,
            input_revision,
            "finished_step = ?6, note_sha256 = ?7, pending_note_sha256s = NULL, failure = NULL,
                note_section_sha256s = ?8",
            &[
                &Step::Written.as_str(),
                &note_sha256,
                &section_lines.join("\n"),
            ],
        )
    }

    // Runs `UPDATE meetings SET <set_clause>` for the lease holder on `input_revision`, and renews
    // the lease. Positions ?1 to ?5 are taken; `extra` binds from ?6.
    fn update_leased(
        &self,
        lease: &RunLease,
        input_revision: i64,
        set_clause: &str,
        extra: &[&dyn ToSql],
    ) -> Result<(), StoreError> {
        let sql = format!(
            "UPDATE meetings SET {set_clause}, run_renewed_at_ms = ?4, updated_at = ?5
             WHERE id = ?1 AND run_token = ?2 AND input_revision = ?3"
        );
        match self.execute_leased(lease, input_revision, &sql, extra)? {
            0 => Err(self.why_unchanged(lease)?),
            _ => Ok(()),
        }
    }

    fn release_leased(
        &self,
        lease: &RunLease,
        input_revision: i64,
        set_clause: &str,
        extra: &[&dyn ToSql],
    ) -> Result<LeaseRelease, StoreError> {
        let sql = format!(
            "UPDATE meetings SET {set_clause}, run_token = NULL, run_renewed_at_ms = NULL,
                updated_at = ?5
             WHERE id = ?1 AND run_token = ?2 AND input_revision = ?3 AND ?4 IS NOT NULL"
        );
        if self.execute_leased(lease, input_revision, &sql, extra)? > 0 {
            return Ok(LeaseRelease::Released);
        }
        match self.why_unchanged(lease)? {
            StoreError::Superseded(_) => Ok(LeaseRelease::Superseded),
            lease_lost => Err(lease_lost),
        }
    }

    fn execute_leased(
        &self,
        lease: &RunLease,
        input_revision: i64,
        sql: &str,
        extra: &[&dyn ToSql],
    ) -> Result<usize, StoreError> {
        let now = Timestamp::now();
        let now_ms = now.as_millisecond();
        let now_text = now.to_string();
        let mut bound: Vec<&dyn ToSql> = vec![
            &lease.id.0,
            &lease.token,
            &input_revision,
            &now_ms,
            &now_text,
        ];
        bound.extend_from_slice(extra);
        Ok(self.connection().execute(sql, bound.as_slice())?)
    }

    // Called after a leased update changed no row: either another run holds the meeting or a
    // new capture bumped its revision.
    fn why_unchanged(&self, lease: &RunLease) -> Result<StoreError, StoreError> {
        let run_token: Option<String> = self
            .connection()
            .query_row(
                "SELECT run_token FROM meetings WHERE id = ?1",
                [lease.id.0],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound(lease.id))?;
        Ok(match run_token {
            Some(run_token) if run_token == lease.token => StoreError::Superseded(lease.id),
            Some(_) | None => StoreError::LeaseLost(lease.id),
        })
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        // A panic mid-query leaves no half-applied state: each call is one SQLite statement.
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

// The outer Result carries SQLite read errors; the inner one carries invalid stored values.
/// Each `<section> <sha256>` line, or the first line that is not one.
fn read_section_sha256s(lines: &str) -> Result<BTreeMap<String, String>, String> {
    lines
        .lines()
        .map(|line| {
            line.rsplit_once(' ')
                .map(|(section, sha256)| (section.to_owned(), sha256.to_owned()))
                .ok_or_else(|| line.to_owned())
        })
        .collect()
}

fn read_meeting(row: &Row<'_>) -> rusqlite::Result<Result<Meeting, StoreError>> {
    let id: i64 = row.get("id")?;
    let invalid = |column: &'static str, value: String, valid: &str| StoreError::InvalidColumn {
        id,
        column,
        value,
        valid: valid.to_owned(),
    };
    let timestamp = |column: &'static str, value: String| {
        value
            .parse::<Timestamp>()
            .map_err(|_| invalid(column, value, "RFC 3339 timestamps in UTC"))
    };
    let input_kind_text: String = row.get("input_kind")?;
    let finished_step_text: String = row.get("finished_step")?;
    let retry_count: i64 = row.get("retry_count")?;
    let started_at: String = row.get("started_at")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;
    let input_path: String = row.get("input_path")?;
    let input_revision: i64 = row.get("input_revision")?;
    let title: Option<String> = row.get("title")?;
    let failure: Option<String> = row.get("failure")?;
    let calendar_match_json: Option<String> = row.get("calendar_match_json")?;
    let extraction_json: Option<String> = row.get("extraction_json")?;
    let extraction_provider: Option<String> = row.get("extraction_provider")?;
    let extraction_attempts: i64 = row.get("extraction_attempts")?;
    let extraction_error: Option<String> = row.get("extraction_error")?;
    let next_extraction_at_ms: Option<i64> = row.get("next_extraction_at_ms")?;
    let note_path: Option<String> = row.get("note_path")?;
    let note_sha256: Option<String> = row.get("note_sha256")?;
    let pending_note_sha256s: Option<String> = row.get("pending_note_sha256s")?;
    let note_section_sha256s: Option<String> = row.get("note_section_sha256s")?;

    Ok((|| {
        let input_kind = InputKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == input_kind_text)
            .ok_or_else(|| {
                let valid = InputKind::ALL.map(InputKind::as_str).join(", ");
                invalid("input_kind", input_kind_text.clone(), &valid)
            })?;
        let finished_step = Step::ALL
            .into_iter()
            .find(|step| step.as_str() == finished_step_text)
            .ok_or_else(|| {
                let valid = Step::ALL.map(Step::as_str).join(", ");
                invalid("finished_step", finished_step_text.clone(), &valid)
            })?;
        Ok(Meeting {
            id: MeetingId(id),
            input_kind,
            input_path: input_path.into(),
            input_revision,
            started_at: timestamp("started_at", started_at)?,
            title,
            finished_step,
            failure,
            retry_count: u32::try_from(retry_count)
                .map_err(|_| invalid("retry_count", retry_count.to_string(), "0 to 4294967295"))?,
            calendar_match_json,
            extraction_json,
            extraction_provider,
            extraction_attempts: u32::try_from(extraction_attempts).map_err(|_| {
                invalid(
                    "extraction_attempts",
                    extraction_attempts.to_string(),
                    "0 to 4294967295",
                )
            })?,
            extraction_error,
            next_extraction_at: next_extraction_at_ms
                .map(|ms| {
                    Timestamp::from_millisecond(ms).map_err(|_| {
                        invalid(
                            "next_extraction_at_ms",
                            ms.to_string(),
                            "UTC milliseconds since 1970",
                        )
                    })
                })
                .transpose()?,
            note_path: note_path.map(PathBuf::from),
            note_sha256,
            pending_note_sha256s: pending_note_sha256s
                .iter()
                .flat_map(|hashes| hashes.split(' ').map(str::to_owned))
                .collect(),
            note_section_sha256s: note_section_sha256s
                .as_deref()
                .map(read_section_sha256s)
                .transpose()
                .map_err(|line| invalid("note_section_sha256s", line, "<section> <sha256>"))?
                .unwrap_or_default(),
            created_at: timestamp("created_at", created_at)?,
            updated_at: timestamp("updated_at", updated_at)?,
        })
    })())
}
