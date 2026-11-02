//! What the audio and video helpers both do at the app's `dump_recorder` and `set_debug`.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;

use telemetry::DumpReason;
use telemetry::debug_mode::DebugMode;

/// Sets the log file level from the app's `set_debug`. A value that is not a time turns debug
/// mode off: the app wrote it, so the next change corrects it.
pub(crate) fn follow_debug_mode(until: Option<&str>) {
    let mode =
        DebugMode::from_enabled_until(until, jiff::Timestamp::now()).unwrap_or_else(|error| {
            tracing::warn!(exception.message = %error, "set_debug.bad_until");
            DebugMode::Off
        });
    if let Some(switch) = crate::DEBUG_MODE_SWITCH.get() {
        switch.apply_now(mode);
    }
}

/// The dump threads still running, joined before the process ends.
static RECORDER_DUMPS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Dumps the flight recorder on its own thread: a dump writes up to 8 MB, which would stall the
/// recording loop past what its queues hold.
pub(crate) fn spawn_recorder_dump(path: PathBuf) {
    let spawned = std::thread::Builder::new()
        .name("recorder-dump".to_owned())
        .spawn(
            move || match telemetry::dump_recorder(&path, DumpReason::AppRequest) {
                Ok(line_count) => tracing::info!(
                    file.path = %path.display(),
                    mc.recorder.line_count = line_count,
                    "recorder.dumped"
                ),
                Err(error) => tracing::warn!(
                    file.path = %path.display(),
                    exception.message = %error,
                    "recorder.dump_failed"
                ),
            },
        );
    match spawned {
        Ok(dump) => {
            let mut dumps = RECORDER_DUMPS
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            dumps.retain(|running| !running.is_finished());
            dumps.push(dump);
        }
        Err(error) => {
            tracing::warn!(exception.message = %error, "recorder.dump_thread_failed");
        }
    }
}

/// Waits for the running dumps. The process ends after this, and a dump cut off there loses its
/// last lines.
pub(crate) fn join_recorder_dumps() {
    let dumps = std::mem::take(
        &mut *RECORDER_DUMPS
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    for dump in dumps {
        if dump.join().is_err() {
            tracing::warn!("recorder.dump_panicked");
        }
    }
}
