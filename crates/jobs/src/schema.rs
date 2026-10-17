//! The `meetings` table: its migrations and how a row reads as a `Meeting`.

use std::collections::BTreeMap;

use jiff::Timestamp;
use rusqlite::Row;
use rusqlite_migration::M;

use crate::state::{FailureCause, FailureKind};
use crate::{ClaimedNote, InputKind, Meeting, MeetingId, NoteOwnership, Step, StoreError};

pub(crate) const MIGRATIONS: &[M<'static>] = &[
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
    // Set exactly when `failure` is. The backfill applies `FailureKind::inferred`: an extraction
    // that failed for good holds the same text in `failure` and `extraction_error`.
    M::up(
        "ALTER TABLE meetings ADD COLUMN failure_kind TEXT;
    UPDATE meetings SET failure_kind =
        CASE WHEN failure = extraction_error THEN 'extraction' ELSE 'step' END
        WHERE failure IS NOT NULL;",
    ),
];

pub(crate) const MEETING_COLUMNS: &str = "id, input_kind, input_path, input_revision, \
    started_at, title, finished_step, failure, failure_kind, retry_count, calendar_match_json, \
    extraction_json, extraction_provider, extraction_attempts, extraction_error, \
    next_extraction_at_ms, note_path, note_sha256, pending_note_sha256s, note_section_sha256s, created_at, updated_at";

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

// The outer Result carries SQLite read errors; the inner one carries invalid stored values.
pub(crate) fn read_meeting(row: &Row<'_>) -> rusqlite::Result<Result<Meeting, StoreError>> {
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
    let failure_kind: Option<String> = row.get("failure_kind")?;
    let calendar_match_json: Option<String> = row.get("calendar_match_json")?;
    let extraction_json: Option<String> = row.get("extraction_json")?;
    let extraction_provider: Option<String> = row.get("extraction_provider")?;
    let extraction_attempts: i64 = row.get("extraction_attempts")?;
    let extraction_error: Option<String> = row.get("extraction_error")?;
    let next_extraction_at_ms: Option<i64> = row.get("next_extraction_at_ms")?;
    let note = read_note_ownership(row, &invalid)?;

    Ok((|| {
        let input_kind = parse_stored(
            InputKind::ALL,
            InputKind::as_str,
            "input_kind",
            input_kind_text,
            &invalid,
        )?;
        let finished_step = parse_stored(
            Step::ALL,
            Step::as_str,
            "finished_step",
            finished_step_text,
            &invalid,
        )?;
        let failure = read_failure(
            id,
            failure,
            failure_kind,
            extraction_error.as_deref(),
            &invalid,
        )?;
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
            note: note?,
            created_at: timestamp("created_at", created_at)?,
            updated_at: timestamp("updated_at", updated_at)?,
        })
    })())
}

fn read_failure(
    id: i64,
    failure: Option<String>,
    failure_kind: Option<String>,
    extraction_error: Option<&str>,
    invalid: &impl Fn(&'static str, String, &str) -> StoreError,
) -> Result<Option<FailureCause>, StoreError> {
    match (failure, failure_kind) {
        (None, None) => Ok(None),
        (None, Some(kind_text)) => Err(invalid(
            "failure_kind",
            kind_text,
            "NULL while failure is NULL",
        )),
        (Some(message), None) => {
            // A binary older than migration 4 still writes `failure` alone: the tray app keeps
            // its connection open across an upgrade.
            tracing::warn!(
                meeting.id = id,
                "failure without failure_kind; reading it with the migration 4 rule"
            );
            let kind = FailureKind::inferred(&message, extraction_error);
            Ok(Some(FailureCause { kind, message }))
        }
        (Some(message), Some(kind_text)) => {
            let kind = parse_stored(
                FailureKind::ALL,
                FailureKind::as_str,
                "failure_kind",
                kind_text,
                invalid,
            )?;
            Ok(Some(FailureCause { kind, message }))
        }
    }
}

/// The value of a closed set whose `as_str` is `text`, read from `column`.
fn parse_stored<T: Copy, const N: usize>(
    all: [T; N],
    as_str: fn(T) -> &'static str,
    column: &'static str,
    text: String,
    invalid: &impl Fn(&'static str, String, &str) -> StoreError,
) -> Result<T, StoreError> {
    if let Some(value) = all.into_iter().find(|value| as_str(*value) == text) {
        return Ok(value);
    }
    let valid = all.map(as_str).join(", ");
    Err(invalid(column, text, &valid))
}

fn read_note_ownership(
    row: &Row<'_>,
    invalid: &impl Fn(&'static str, String, &str) -> StoreError,
) -> rusqlite::Result<Result<NoteOwnership, StoreError>> {
    let note_path: Option<String> = row.get("note_path")?;
    let note_sha256: Option<String> = row.get("note_sha256")?;
    let pending_note_sha256s: Option<String> = row.get("pending_note_sha256s")?;
    let note_section_sha256s: Option<String> = row.get("note_section_sha256s")?;

    let Some(note_path) = note_path else {
        let hash_without_path = [
            ("note_sha256", note_sha256),
            ("pending_note_sha256s", pending_note_sha256s),
            ("note_section_sha256s", note_section_sha256s),
        ]
        .into_iter()
        .find_map(|(column, value)| value.map(|value| (column, value)));
        return Ok(match hash_without_path {
            Some((column, value)) => Err(invalid(column, value, "NULL while note_path is NULL")),
            None => Ok(NoteOwnership { claimed: None }),
        });
    };
    let section_sha256s = match note_section_sha256s.as_deref().map(read_section_sha256s) {
        Some(Ok(section_sha256s)) => section_sha256s,
        Some(Err(line)) => {
            return Ok(Err(invalid(
                "note_section_sha256s",
                line,
                "<section> <sha256>",
            )));
        }
        None => BTreeMap::new(),
    };
    Ok(Ok(NoteOwnership {
        claimed: Some(ClaimedNote {
            path: note_path.into(),
            written_sha256: note_sha256,
            pending_sha256s: pending_note_sha256s
                .iter()
                .flat_map(|hashes| hashes.split(' ').map(str::to_owned))
                .collect(),
            section_sha256s,
        }),
    }))
}
