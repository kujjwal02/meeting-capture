//! Runs many meetings at once: the periodic sweep of unfinished meetings and the retry of every
//! failed one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use jobs::{MeetingId, RunOutcome};

use crate::{Pipeline, PipelineError};

// Two, so one slow run does not hold up the rest. Saving a provider can queue every old note at
// once, and that many LLM calls in parallel would hit the rate limit together and then retry
// together. The cap holds per batch kind, so a sweep and a retry together run at most four
// meetings. Single runs from captions, huddles and the tray run beside them.
const WORKERS: usize = 2;

/// Marks a batch as running for as long as it lives. Dropped on a worker panic too, so a crashed
/// batch cannot block every later one.
struct RunningBatch<'a>(&'a AtomicBool);

impl<'a> RunningBatch<'a> {
    /// Returns `None` while another batch holds the flag.
    fn start(is_running: &'a AtomicBool) -> Option<Self> {
        (!is_running.swap(true, Ordering::AcqRel)).then_some(Self(is_running))
    }
}

impl Drop for RunningBatch<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Pipeline {
    /// Runs every unfinished meeting and due extraction retry on a fixed pool of worker threads,
    /// and reports each run to `on_run` on the worker that ran it. Logs and returns at once while
    /// another sweep on this pipeline runs.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Store` when the unfinished meetings cannot be listed.
    pub fn sweep(
        &self,
        on_run: impl Fn(MeetingId, &Result<RunOutcome, PipelineError>) + Sync,
    ) -> Result<(), PipelineError> {
        let Some(_sweeping) = RunningBatch::start(&self.is_sweeping) else {
            tracing::info!("the last sweep is still running; skipping this one");
            return Ok(());
        };
        let unfinished = self.store.unfinished()?;
        self.run_on_workers(unfinished.into_iter().map(|meeting| meeting.id), on_run);
        Ok(())
    }

    /// Runs every failed meeting again from the step that failed, on its own worker pool, and
    /// reports each run to `on_run`. Runs even while a sweep runs, since a user asked for it.
    /// Logs and returns at once while another retry on this pipeline runs, because that retry
    /// already listed every failed meeting.
    ///
    /// # Errors
    ///
    /// Returns `PipelineError::Store` when the failed meetings cannot be listed.
    pub fn retry_failed(
        &self,
        on_run: impl Fn(MeetingId, &Result<RunOutcome, PipelineError>) + Sync,
    ) -> Result<(), PipelineError> {
        let Some(_retrying) = RunningBatch::start(&self.is_retrying) else {
            tracing::info!("the last retry of failed meetings is still running; skipping this one");
            return Ok(());
        };
        let failed = self.store.failed()?;
        self.run_on_workers(failed.into_iter().map(|meeting| meeting.id), on_run);
        Ok(())
    }

    // Blocks until every meeting has run. A panic in a worker reaches the caller once the other
    // worker has drained the queue.
    fn run_on_workers(
        &self,
        ids: impl Iterator<Item = MeetingId> + Send,
        on_run: impl Fn(MeetingId, &Result<RunOutcome, PipelineError>) + Sync,
    ) {
        let queue = Mutex::new(ids);
        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                scope.spawn(|| {
                    loop {
                        let next = queue.lock().unwrap_or_else(PoisonError::into_inner).next();
                        let Some(id) = next else {
                            return;
                        };
                        on_run(id, &self.run(id));
                    }
                });
            }
        });
    }
}
