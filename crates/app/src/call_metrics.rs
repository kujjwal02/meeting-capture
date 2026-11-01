//! The metrics and environment rows of the call that records now. Each audio heartbeat brings
//! one second of stem levels, written as its own row stamped when it arrives. The call's scope
//! samples the host gauges and video frame counts into `metrics.jsonl` in the call's debug
//! folder, one row a second. The mic's first stream and each mic change write an `env.jsonl` row
//! with the route's reason.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use capture_protocol::{AudioReport, MicRoute, Stem, StemLevel, VideoReport};
use jiff::Timestamp;
use serde_json::Value;
use tauri::{AppHandle, Manager as _};
use telemetry::env::{AudioDevice, ChosenMic, EnvReason};
use telemetry::metrics::{Counter, MetricsScope, RowPoints};
use telemetry::trace::TraceId;

use crate::AppState;
use crate::host_metrics::{HostGauges, HostSample};

const CALL_INTERVAL_MS: u32 = 1_000;
/// A rows-only scope no row used for this long is closed. mc-meet posts every second while its
/// call is live, so a gap this long means the call is over.
const ROWS_ONLY_IDLE: Duration = Duration::from_secs(120);

static OPEN_CALLS: Mutex<Vec<CallMetrics>> = Mutex::new(Vec::new());

/// Scopes for mc-meet's rows of calls that record no audio, and so have no open call scope.
static ROWS_ONLY_CALLS: Mutex<Vec<RowsOnlyScope>> = Mutex::new(Vec::new());

struct CallMetrics {
    call_id: PathBuf,
    folder: PathBuf,
    /// Shared so a writer takes it out of the lock before its file write.
    scope: Arc<MetricsScope>,
    /// Made on the first video heartbeat, so an audio-only call has no video points.
    video: Option<VideoCounters>,
    host: HostGauges,
    /// The mic and route of the last environment row, `None` before the first.
    last_mic: Option<MicEnv>,
}

#[derive(Clone, PartialEq)]
struct MicEnv {
    device: Option<String>,
    route: Option<MicRoute>,
}

struct RowsOnlyScope {
    call_id: PathBuf,
    scope: Arc<MetricsScope>,
    used_at: Instant,
}

struct VideoCounters {
    encoded: Counter,
    repeated: Counter,
    /// The last heartbeat's segment and counts. A new segment starts its counts from zero.
    last: Option<VideoTally>,
}

#[derive(Clone, Copy)]
struct VideoTally {
    segment: u32,
    encoded: u64,
    repeated: u64,
}

/// The call's rows while this lives.
pub(crate) struct OpenCallMetrics {
    call_id: PathBuf,
}

/// Opens the call's metrics scope. The rows stop when the returned value is dropped.
pub(crate) fn open_call(app: &AppHandle, call_id: &Path) -> OpenCallMetrics {
    let folder = app.state::<AppState>().call_debug.call_folder(call_id);
    let scope = MetricsScope::open(&folder, CALL_INTERVAL_MS)
        .with_trace_id(TraceId::for_call(&call_id.display().to_string()));
    lock().push(CallMetrics {
        call_id: call_id.to_path_buf(),
        folder,
        scope: Arc::new(scope),
        video: None,
        host: HostGauges::default(),
        last_mic: None,
    });
    OpenCallMetrics {
        call_id: call_id.to_path_buf(),
    }
}

impl Drop for OpenCallMetrics {
    fn drop(&mut self) {
        let closed: Vec<CallMetrics> = {
            let mut calls = lock();
            let (closed, open) = std::mem::take(&mut *calls)
                .into_iter()
                .partition(|call| call.call_id == self.call_id);
            *calls = open;
            closed
        };
        if closed.is_empty() {
            return;
        }
        // Dropping a scope writes its last row and joins its sampler thread, so it happens off
        // the async runtime and after the lock.
        tauri::async_runtime::spawn_blocking(move || drop(closed));
    }
}

fn lock() -> std::sync::MutexGuard<'static, Vec<CallMetrics>> {
    OPEN_CALLS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The scope of the call when its rows are open.
fn call_scope(call_id: &Path) -> Option<Arc<MetricsScope>> {
    lock()
        .iter()
        .find(|call| call.call_id == call_id)
        .map(|call| Arc::clone(&call.scope))
}

/// Writes mc-meet's rows into the call's scope. A call that records no audio has no open scope,
/// so its rows go to a rows-only scope on the call's folder, kept while rows come.
pub(crate) fn write_mc_meet_rows(call_id: &Path, folder: &Path, rows: &[(Timestamp, RowPoints)]) {
    if rows.is_empty() {
        return;
    }
    let scope = call_scope(call_id).unwrap_or_else(|| rows_only_scope(call_id, folder));
    for (at, points) in rows {
        scope.write_mc_meet_row(*at, points);
    }
}

fn rows_only_scope(call_id: &Path, folder: &Path) -> Arc<MetricsScope> {
    let mut scopes = ROWS_ONLY_CALLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let now = Instant::now();
    // A rows-only scope has no sampler, so dropping one under the lock writes nothing.
    scopes.retain(|rows_only| now.duration_since(rows_only.used_at) < ROWS_ONLY_IDLE);
    if let Some(rows_only) = scopes
        .iter_mut()
        .find(|rows_only| rows_only.call_id == call_id)
    {
        rows_only.used_at = now;
        return Arc::clone(&rows_only.scope);
    }
    let scope = Arc::new(
        MetricsScope::open_rows(folder, CALL_INTERVAL_MS)
            .with_trace_id(TraceId::for_call(&call_id.display().to_string())),
    );
    scopes.push(RowsOnlyScope {
        call_id: call_id.to_path_buf(),
        scope: Arc::clone(&scope),
        used_at: now,
    });
    scope
}

/// Sets the host gauges in every open call's rows.
pub(crate) fn set_host_samples(samples: &[HostSample]) {
    for call in lock().iter_mut() {
        call.host.set_all(&call.scope, samples);
    }
}

/// Takes the levels from a heartbeat, and writes an environment row when the mic starts or
/// changes.
pub(crate) fn take_audio_report(app: &AppHandle, call_id: &Path, report: &AudioReport) {
    let mic_env = match report {
        AudioReport::Heartbeat {
            mic_level,
            remote_level,
            ..
        } => {
            write_levels(call_id, mic_level.as_ref(), remote_level.as_ref());
            None
        }
        AudioReport::Recording {
            stream: Stem::Mic,
            device,
            mic_route,
            ..
        } => Some((Some(device.as_str()), mic_route.as_ref())),
        AudioReport::DeviceChanged {
            stream: Stem::Mic,
            new_device,
            mic_route,
            ..
        } => Some((Some(new_device.as_str()), mic_route.as_ref())),
        // A call that starts with no safe mic still gets its start row, with no mic in it.
        AudioReport::MicUnavailable { .. } => Some((None, None)),
        AudioReport::Recording {
            stream: Stem::Remote,
            ..
        }
        | AudioReport::DeviceChanged {
            stream: Stem::Remote,
            ..
        }
        | AudioReport::Gap { .. }
        | AudioReport::Rotated { .. }
        | AudioReport::Permission { .. }
        | AudioReport::SystemSleep { .. }
        | AudioReport::SystemWake
        | AudioReport::Stopped { .. }
        | AudioReport::Error { .. } => None,
    };
    if let Some((device, route)) = mic_env {
        let mic = MicEnv {
            device: device.map(str::to_owned),
            route: route.cloned(),
        };
        note_mic_env(app, call_id, mic);
    }
}

/// One row of the heartbeat's levels, stamped when it arrives. Gauges sampled on the scope's own
/// grid would drop or double a level whenever a heartbeat lands near a row's edge.
fn write_levels(call_id: &Path, mic: Option<&StemLevel>, remote: Option<&StemLevel>) {
    let Some(scope) = call_scope(call_id) else {
        return;
    };
    let mut points = RowPoints::default();
    for (stem, level) in [(Stem::Mic, mic), (Stem::Remote, remote)] {
        let Some(level) = level else {
            continue;
        };
        let attributes = [("mc.audio.stem", Value::from(stem.as_str()))];
        points.gauge("mc.audio.level.rms", "dBFS", &attributes, level.rms_dbfs);
        points.gauge("mc.audio.level.peak", "dBFS", &attributes, level.peak_dbfs);
    }
    scope.write_row(Timestamp::now(), &points);
}

/// Writes an environment row for the call's first mic, and for each later mic or route that
/// differs from the last row's. A new segment on the same mic writes none.
fn note_mic_env(app: &AppHandle, call_id: &Path, mic: MicEnv) {
    let target = lock()
        .iter_mut()
        .find(|call| call.call_id == call_id)
        .and_then(|call| {
            let reason = match &call.last_mic {
                None => EnvReason::CallStart,
                Some(last) if *last == mic => return None,
                Some(_) => EnvReason::Change,
            };
            call.last_mic = Some(mic.clone());
            Some((reason, call.folder.clone()))
        });
    let Some((reason, folder)) = target else {
        return;
    };
    // The snapshot reads permissions and the OS, and the write is file IO: neither belongs on
    // the async runtime that reads the helper's reports.
    let app = app.clone();
    let call_id = call_id.to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        write_mic_env(&app, &call_id, &folder, reason, &mic);
    });
}

fn write_mic_env(app: &AppHandle, call_id: &Path, folder: &Path, reason: EnvReason, mic: &MicEnv) {
    let mut snapshot = crate::host_metrics::env_snapshot(app);
    if let Some(route) = &mic.route {
        snapshot.audio_input_default = Some(AudioDevice {
            name: route.default_input.clone(),
            transport: Some(route.default_input_transport.as_str()),
        });
        snapshot.audio_output_default = route.default_output.clone().map(|name| AudioDevice {
            name: Some(name),
            transport: None,
        });
        snapshot.mic_chosen = Some(ChosenMic {
            name: mic.device.clone(),
            reason: route.reason.as_str(),
        });
    }
    let log_dir = app.state::<AppState>().settings.app_dirs().log_dir();
    if let Err(error) = snapshot.write(reason, &log_dir, Some(folder)) {
        tracing::warn!(
            call.id = %call_id.display(),
            exception.message = %error,
            "could not write the environment row"
        );
    }
}

/// Adds the frames a video heartbeat counted since the last one.
pub(crate) fn take_video_report(call_id: &Path, report: &VideoReport) {
    match report {
        VideoReport::Heartbeat {
            segment,
            frames_encoded,
            frames_repeated,
            ..
        } => {
            let tally = VideoTally {
                segment: *segment,
                encoded: frames_encoded.saturating_sub(*frames_repeated),
                repeated: *frames_repeated,
            };
            if let Some(call) = lock().iter_mut().find(|call| call.call_id == call_id) {
                let scope = &call.scope;
                call.video
                    .get_or_insert_with(|| VideoCounters::new(scope))
                    .take(tally);
            }
        }
        VideoReport::Recording { .. }
        | VideoReport::Gap { .. }
        | VideoReport::WindowChanged { .. }
        | VideoReport::Permission { .. }
        | VideoReport::Stopped { .. }
        | VideoReport::Error { .. } => {}
    }
}

impl VideoCounters {
    fn new(scope: &MetricsScope) -> Self {
        Self {
            encoded: scope.counter(
                "mc.video.frames",
                "{frame}",
                &[("mc.video.frame.kind", "encoded")],
            ),
            repeated: scope.counter(
                "mc.video.frames",
                "{frame}",
                &[("mc.video.frame.kind", "repeated")],
            ),
            last: None,
        }
    }

    fn take(&mut self, tally: VideoTally) {
        // A restarted helper or a new segment counts from zero again.
        let since = self
            .last
            .filter(|last| last.segment == tally.segment)
            .unwrap_or(VideoTally {
                segment: tally.segment,
                encoded: 0,
                repeated: 0,
            });
        self.encoded
            .add(tally.encoded.saturating_sub(since.encoded));
        self.repeated
            .add(tally.repeated.saturating_sub(since.repeated));
        self.last = Some(tally);
    }
}
