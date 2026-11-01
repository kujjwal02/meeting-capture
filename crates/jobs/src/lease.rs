//! The run lease: one run per meeting across processes, renewed while the run lasts, and given
//! back however the run ends.

use std::cell::Cell;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use rusqlite::{OptionalExtension, ToSql, params};

use crate::state::FailureKind;
use crate::{JobStore, Meeting, MeetingId, StoreError};

// A lease not renewed for this long belongs to a process that died mid-run. Every step update
// renews it, and `JobStore::run` renews it from a second thread well within this time.
const LEASE_EXPIRY: Duration = Duration::from_mins(2);

#[expect(clippy::cast_possible_truncation, reason = "two minutes fits in i64")]
const LEASE_EXPIRY_MS: i64 = LEASE_EXPIRY.as_millis() as i64;

// Longer than any step should take, LLM call included.
const MAX_RUN_TIME: Duration = Duration::from_mins(15);

/// How a call to `JobStore::run` ended.
#[derive(Debug)]
pub enum RunOutcome {
    Finished(Box<Meeting>),
    /// Another run, in this process or another, holds the meeting. It picks up any new capture
    /// before it lets go.
    AlreadyRunning,
}

/// An error a run's steps return. It wraps the store errors of step updates, and `JobStore::run`
/// records its text as the meeting's failure.
pub trait StepError: From<StoreError> + fmt::Display {
    /// The error for steps that panicked with `message`. It is recorded as the failure.
    fn panicked(message: String) -> Self;
}

/// One attempt at a meeting's steps under its lease, on the input the attempt started from.
/// Every update checks both, so a run on an older input changes nothing.
pub struct LeasedRun<'a> {
    pub(crate) store: &'a JobStore,
    lease: &'a RunLease,
    meeting: Meeting,
    // Set when an update or renewal finds the run must stop. `JobStore::run` reads it, so a
    // superseded run starts over and a run that lost its lease records nothing, however the
    // steps wrap the store error.
    stop: Cell<Option<LeaseStop>>,
}

impl LeasedRun<'_> {
    /// The meeting as it was when this attempt started.
    #[must_use]
    pub fn meeting(&self) -> &Meeting {
        &self.meeting
    }

    /// Checks that this attempt still holds the lease and that no new capture superseded its
    /// input, and renews the lease. A step calls it just before a write outside the store.
    ///
    /// # Errors
    ///
    /// Returns `StoreError::LeaseLost` when another run took the meeting, and
    /// `StoreError::Superseded` when a new capture arrived during the run.
    pub fn confirm_current_input(&self) -> Result<(), StoreError> {
        // The check changes nothing the tray shows, so `updated_at` (?5) stays as it was.
        let renewed = self.execute_leased(
            "UPDATE meetings SET run_renewed_at_ms = ?4
             WHERE id = ?1 AND run_token = ?2 AND input_revision = ?3 AND ?5 IS NOT NULL",
            &[],
        )?;
        self.stop_unless_changed(renewed)
    }

    // Runs `UPDATE meetings SET <set_clause>` for the lease holder on this attempt's input
    // revision, and renews the lease. Positions ?1 to ?5 are taken; `extra` binds from ?6.
    pub(crate) fn update(&self, set_clause: &str, extra: &[&dyn ToSql]) -> Result<(), StoreError> {
        let sql = format!(
            "UPDATE meetings SET {set_clause}, run_renewed_at_ms = ?4, updated_at = ?5
             WHERE id = ?1 AND run_token = ?2 AND input_revision = ?3"
        );
        let updated = self.execute_leased(&sql, extra)?;
        self.stop_unless_changed(updated)
    }

    // A leased update that changed no row means the run must stop.
    fn stop_unless_changed(&self, changed_rows: usize) -> Result<(), StoreError> {
        match changed_rows {
            0 => Err(self.record_stop(self.why_unchanged()?)),
            _ => Ok(()),
        }
    }

    fn record_stop(&self, stop: LeaseStop) -> StoreError {
        self.stop.set(Some(stop));
        stop.error(self.lease.id)
    }

    // Gives the lease back if the meeting is still on this attempt's input revision.
    fn release(&self) -> Result<LeaseRelease, StoreError> {
        self.release_leased("failure = failure", &[])
    }

    // Records a failed run and gives the lease back, in one step, so a capture that arrives in
    // between is never left without a run.
    fn fail(&self, failure: &str) -> Result<LeaseRelease, StoreError> {
        self.release_leased(
            "failure = ?6, failure_kind = ?7",
            &[&failure, &FailureKind::Step.as_str()],
        )
    }

    fn release_leased(
        &self,
        set_clause: &str,
        extra: &[&dyn ToSql],
    ) -> Result<LeaseRelease, StoreError> {
        let sql = format!(
            "UPDATE meetings SET {set_clause}, run_token = NULL, run_renewed_at_ms = NULL,
                updated_at = ?5
             WHERE id = ?1 AND run_token = ?2 AND input_revision = ?3"
        );
        if self.execute_leased(&sql, extra)? > 0 {
            return Ok(LeaseRelease::Released);
        }
        match self.why_unchanged()? {
            LeaseStop::Superseded => Ok(LeaseRelease::Superseded),
            LeaseStop::LeaseLost => Err(LeaseStop::LeaseLost.error(self.lease.id)),
        }
    }

    // Binds ?1 to ?5, then `extra`. rusqlite refuses more values than the highest position a
    // statement names, so a statement with no use for ?5 names it in the always-true clause
    // `?5 IS NOT NULL`.
    fn execute_leased(&self, sql: &str, extra: &[&dyn ToSql]) -> Result<usize, StoreError> {
        let now = Timestamp::now();
        let now_ms = now.as_millisecond();
        let now_text = now.to_string();
        let mut bound: Vec<&dyn ToSql> = vec![
            &self.lease.id.0,
            &self.lease.token,
            &self.meeting.input_revision,
            &now_ms,
            &now_text,
        ];
        bound.extend_from_slice(extra);
        Ok(self.store.connection().execute(sql, bound.as_slice())?)
    }

    // Called after a leased update changed no row: either another run holds the meeting or a
    // new capture bumped its revision.
    fn why_unchanged(&self) -> Result<LeaseStop, StoreError> {
        let run_token: Option<String> = self
            .store
            .connection()
            .query_row(
                "SELECT run_token FROM meetings WHERE id = ?1",
                [self.lease.id.0],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::NotFound(self.lease.id))?;
        Ok(match run_token {
            Some(run_token) if run_token == self.lease.token => LeaseStop::Superseded,
            Some(_) | None => LeaseStop::LeaseLost,
        })
    }
}

impl JobStore {
    /// Runs `steps` under the meeting's lease, renewed from a second thread for up to 15
    /// minutes. A capture of the same meeting during the run makes the steps start over on the
    /// new input. A returned error or a panic is recorded as the meeting's failure, unless the
    /// run lost its lease to another run. The lease is given back however the run ends.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the run: a step's, or a store error around the steps.
    /// Returns `StoreError::NotFound` for an unknown id.
    pub fn run<E: StepError>(
        &self,
        id: MeetingId,
        mut steps: impl FnMut(&LeasedRun<'_>) -> Result<(), E>,
    ) -> Result<RunOutcome, E> {
        let Some(lease) = self.lease_run(id)? else {
            tracing::info!("another run holds this meeting");
            return Ok(RunOutcome::AlreadyRunning);
        };
        let (stop_renewing, is_run_over) = mpsc::channel::<()>();
        let renewed_lease = lease.clone();
        let mut held = HeldLease {
            store: self,
            lease: Some(lease),
        };
        std::thread::scope(|scope| {
            scope.spawn(move || self.keep_lease_alive(&renewed_lease, &is_run_over));
            let outcome = self.run_leased(&mut held, &mut steps);
            drop(stop_renewing);
            outcome
        })
    }

    fn run_leased<E: StepError>(
        &self,
        held: &mut HeldLease<'_>,
        steps: &mut impl FnMut(&LeasedRun<'_>) -> Result<(), E>,
    ) -> Result<RunOutcome, E> {
        let id = held.lease().id;
        loop {
            let run = LeasedRun {
                store: self,
                lease: held.lease(),
                meeting: self.get(id)?,
                stop: Cell::new(None),
            };
            // The meeting and its input revision start the attempt's trace, so a rerun on a new
            // capture is a trace of its own.
            let _attempt = tracing::info_span!(
                "pipeline.attempt",
                meeting.id = %id,
                mc.input.revision = run.meeting.input_revision
            )
            .entered();
            // A panic in a step becomes a recorded failure, so the sweep does not rerun the
            // meeting forever and the tray shows the cause.
            let steps_outcome = std::panic::catch_unwind(AssertUnwindSafe(|| steps(&run)))
                .unwrap_or_else(|panic| Err(E::panicked(panic_message(panic.as_ref()))));
            let release = match steps_outcome {
                Ok(()) => run.release()?,
                Err(error) => match run.stop.get() {
                    Some(LeaseStop::Superseded) => continue,
                    Some(LeaseStop::LeaseLost) => return Err(error),
                    None => {
                        tracing::error!(exception.message = %error, "pipeline run failed");
                        match run.fail(&error.to_string()) {
                            Ok(LeaseRelease::Released) => held.released(),
                            Ok(LeaseRelease::Superseded) => continue,
                            Err(store_error) => tracing::error!(
                                exception.message = %store_error,
                                "could not record the failure"
                            ),
                        }
                        return Err(error);
                    }
                },
            };
            match release {
                LeaseRelease::Released => {
                    held.released();
                    return Ok(RunOutcome::Finished(Box::new(self.get(id)?)));
                }
                LeaseRelease::Superseded => {}
            }
        }
    }

    // A step such as the LLM call can outlast the lease. Renewing from a second thread keeps the
    // lease short, so a crashed process frees its meetings within `LEASE_EXPIRY`. Renewal stops
    // after `MAX_RUN_TIME`, so a hung run frees its meeting too; a step that writes outside the
    // store calls `LeasedRun::confirm_current_input` first, so neither the hung run nor a run on
    // an older input overwrites a later run's output.
    fn keep_lease_alive(&self, lease: &RunLease, is_run_over: &Receiver<()>) {
        let _span = tracing::info_span!("pipeline.lease", meeting.id = %lease.id).entered();
        let renew_until = Instant::now() + MAX_RUN_TIME;
        while let Err(RecvTimeoutError::Timeout) = is_run_over.recv_timeout(LEASE_EXPIRY / 4) {
            if Instant::now() > renew_until {
                tracing::error!("run is still going after 15 minutes; letting the lease expire");
                return;
            }
            match self.renew_lease(lease) {
                Ok(true) => {}
                // The run gave the lease back between two checks.
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!(exception.message = %error, "could not renew the run lease");
                }
            }
        }
    }

    // Takes the run lease for a meeting. Returns `None` while another run holds it. An attempt
    // after a failure counts as a retry; the failure stays visible until a step succeeds.
    fn lease_run(&self, id: MeetingId) -> Result<Option<RunLease>, StoreError> {
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

    // Returns whether `lease` still held the meeting, and so was renewed.
    fn renew_lease(&self, lease: &RunLease) -> Result<bool, StoreError> {
        let renewed = self.connection().execute(
            "UPDATE meetings SET run_renewed_at_ms = ?3 WHERE id = ?1 AND run_token = ?2",
            params![lease.id.0, lease.token, Timestamp::now().as_millisecond()],
        )?;
        Ok(renewed > 0)
    }

    // Gives the lease back whatever the revision, when a run stops on a store error or a panic.
    fn abandon_run(&self, lease: &RunLease) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE meetings SET run_token = NULL, run_renewed_at_ms = NULL
             WHERE id = ?1 AND run_token = ?2",
            params![lease.id.0, lease.token],
        )?;
        Ok(())
    }
}

// Proof that this process runs the steps for one meeting. A clone lets a second thread keep the
// lease alive.
#[derive(Debug, Clone)]
struct RunLease {
    id: MeetingId,
    token: String,
}

// Why a step update or renewal stopped the run.
#[derive(Debug, Clone, Copy)]
enum LeaseStop {
    // A new capture arrived during the run. The lease is still held; run again.
    Superseded,
    LeaseLost,
}

impl LeaseStop {
    fn error(self, id: MeetingId) -> StoreError {
        match self {
            LeaseStop::Superseded => StoreError::Superseded(id),
            LeaseStop::LeaseLost => StoreError::LeaseLost(id),
        }
    }
}

// How giving back a lease went.
#[derive(Debug, PartialEq, Eq)]
enum LeaseRelease {
    Released,
    // A new capture arrived during the run. The lease is still held; run again.
    Superseded,
}

// Gives the lease back however the run ends, a panic included, so a tray thread that dies does
// not hold the meeting until the lease expires.
struct HeldLease<'a> {
    store: &'a JobStore,
    lease: Option<RunLease>,
}

impl HeldLease<'_> {
    fn lease(&self) -> &RunLease {
        self.lease
            .as_ref()
            .expect("a released lease is never used again")
    }

    fn released(&mut self) {
        self.lease = None;
    }
}

impl Drop for HeldLease<'_> {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take()
            && let Err(error) = self.store.abandon_run(&lease)
        {
            tracing::error!(exception.message = %error, "could not give back the run lease");
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
        (Some(message), _) => (*message).to_owned(),
        (None, Some(message)) => message.clone(),
        (None, None) => "a panic with no message".to_owned(),
    }
}
