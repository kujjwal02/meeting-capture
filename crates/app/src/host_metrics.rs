//! Host gauges every 10 s: CPU and memory of the app and of each capture helper, and free space
//! on the data folder's disk. Rows go to the day's metrics file, and into the call's rows while a
//! call records. Also writes the environment row at app start.
//!
//! `sysinfo` reads the same numbers on Mac and Windows, so neither needs code of its own here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use sysinfo::{DiskRefreshKind, Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tauri::{AppHandle, Manager as _};
use telemetry::env::{EnvReason, EnvSnapshot};
use telemetry::metrics::{Gauge, MetricsScope};

use crate::AppState;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
const SAMPLE_INTERVAL_MS: u32 = 10_000;
/// Samples sit on a 10 s grid that starts half an interval after the day's scope opens. The
/// scope writes its rows on a 10 s grid from its opening, so each sample lands mid-row and timer
/// jitter never puts two samples in one row or none in another.
const FIRST_SAMPLE_DELAY: Duration = Duration::from_secs(5);

/// Capture helpers alive now: pid and subcommand.
static HELPERS: Mutex<Vec<(u32, &'static str)>> = Mutex::new(Vec::new());

/// A point's attribute names and values.
pub(crate) type HostAttributes = Vec<(&'static str, &'static str)>;

/// One host gauge's value from a sample.
pub(crate) struct HostSample {
    pub name: &'static str,
    pub unit: &'static str,
    pub attributes: HostAttributes,
    pub value: f64,
}

/// A scope's host gauges, made the first time each shows up.
#[derive(Default)]
pub(crate) struct HostGauges {
    gauges: HashMap<(&'static str, HostAttributes), Gauge>,
}

impl HostGauges {
    pub(crate) fn set_all(&mut self, scope: &MetricsScope, samples: &[HostSample]) {
        for sample in samples {
            self.gauges
                .entry((sample.name, sample.attributes.clone()))
                .or_insert_with(|| scope.gauge(sample.name, sample.unit, &sample.attributes))
                .set(sample.value);
        }
    }
}

/// A capture helper the host gauges follow until this is dropped.
pub(crate) struct TrackedHelper {
    pid: u32,
}

/// Follows the helper with `pid` until the returned value is dropped.
pub(crate) fn track_helper(pid: u32, subcommand: &'static str) -> TrackedHelper {
    HELPERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((pid, subcommand));
    TrackedHelper { pid }
}

impl Drop for TrackedHelper {
    fn drop(&mut self) {
        HELPERS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(pid, _)| *pid != self.pid);
    }
}

/// Writes the app-start environment row, then samples the host every 10 s for the app's life.
pub(crate) fn start(app: &AppHandle) {
    let app_dirs = app.state::<AppState>().settings.app_dirs().clone();
    let log_dir = app_dirs.log_dir();
    let start_app = app.clone();
    let start_log_dir = log_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) =
            env_snapshot(&start_app).write(EnvReason::AppStart, &start_log_dir, None)
        {
            tracing::warn!(exception.message = %error, "could not write the environment row");
        }
    });
    let data_dir = app_dirs.calls_dir();
    let spawned = std::thread::Builder::new()
        .name("host-metrics".to_owned())
        .spawn(move || sample_forever(&log_dir, &data_dir));
    if let Err(error) = spawned {
        tracing::warn!(exception.message = %error, "could not start the host metrics");
    }
}

fn sample_forever(log_dir: &Path, data_dir: &Path) {
    let opened_at = Instant::now();
    let scope = MetricsScope::open_daily(log_dir, SAMPLE_INTERVAL_MS);
    let mut gauges = HostGauges::default();
    let mut probe = match HostProbe::new(data_dir) {
        Ok(probe) => probe,
        Err(error) => {
            tracing::warn!(
                exception.message = %error,
                "host metrics are unsupported on this system"
            );
            return;
        }
    };
    let mut next_sample_at = opened_at + FIRST_SAMPLE_DELAY;
    loop {
        std::thread::sleep(next_sample_at.saturating_duration_since(Instant::now()));
        let samples = probe.sample();
        gauges.set_all(&scope, &samples);
        crate::call_metrics::set_host_samples(&samples);
        next_sample_at += SAMPLE_INTERVAL;
        // A Mac that slept skips the samples it missed instead of taking them all at once.
        let now = Instant::now();
        while next_sample_at <= now {
            next_sample_at += SAMPLE_INTERVAL;
        }
    }
}

struct HostProbe {
    system: System,
    disks: Disks,
    data_dir: PathBuf,
    app_pid: u32,
    cpu_count: f64,
    /// The processes the last sample found, which have a CPU time to measure the next from.
    sampled_pids: HashSet<u32>,
}

impl HostProbe {
    fn new(data_dir: &Path) -> Result<Self, sysinfo::Error> {
        let cpu_count = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Ok(Self {
            system: System::new()?,
            disks: Disks::new()?,
            data_dir: data_dir.to_path_buf(),
            app_pid: std::process::id(),
            cpu_count: f64::from(u32::try_from(cpu_count).unwrap_or(u32::MAX)),
            sampled_pids: HashSet::new(),
        })
    }

    fn sample(&mut self) -> Vec<HostSample> {
        let mut processes = vec![(self.app_pid, "app", None)];
        processes.extend(
            HELPERS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .map(|(pid, subcommand)| (*pid, "mc-capture", Some(*subcommand))),
        );
        // sysinfo removes a dead process only when asked to refresh it, so the refresh also
        // names every process it still holds: a helper that exited leaves it then.
        let mut pids: Vec<Pid> = processes
            .iter()
            .map(|(pid, _, _)| Pid::from_u32(*pid))
            .collect();
        pids.extend(self.system.processes().keys().copied());
        pids.sort_unstable();
        pids.dedup();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let mut samples = Vec::new();
        let mut sampled_pids = HashSet::new();
        for (pid, service_name, subcommand) in processes {
            let Some(process) = self.system.process(Pid::from_u32(pid)) else {
                continue;
            };
            sampled_pids.insert(pid);
            let mut attributes = vec![("service.name", service_name)];
            if let Some(subcommand) = subcommand {
                attributes.push(("process.command", subcommand));
            }
            // A process's first refresh has no earlier CPU time to measure from, so its usage
            // reads as 0 or as its whole life's average.
            if self.sampled_pids.contains(&pid) {
                // sysinfo gives percent of one core; OpenTelemetry's utilization is a share of
                // all.
                samples.push(HostSample {
                    name: "process.cpu.utilization",
                    unit: "1",
                    attributes: attributes.clone(),
                    value: f64::from(process.cpu_usage()) / 100.0 / self.cpu_count,
                });
            }
            samples.push(HostSample {
                name: "process.memory.usage",
                unit: "By",
                attributes,
                value: bytes_value(process.memory()),
            });
        }
        self.sampled_pids = sampled_pids;
        self.disks
            .refresh_specifics(true, DiskRefreshKind::nothing().with_storage());
        let data_disk = self
            .disks
            .iter()
            .filter(|disk| self.data_dir.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().components().count());
        if let Some(disk) = data_disk {
            samples.push(HostSample {
                name: "system.filesystem.free",
                unit: "By",
                attributes: Vec::new(),
                value: bytes_value(disk.available_space()),
            });
        }
        samples
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "byte counts below 2^53, 8 PB, are exact"
)]
fn bytes_value(bytes: u64) -> f64 {
    bytes as f64
}

/// What the app can say about the machine: the OS version and the permissions it knows. Audio
/// devices come from the capture helper's reports.
pub(crate) fn env_snapshot(app: &AppHandle) -> EnvSnapshot {
    let mut snapshot = EnvSnapshot {
        os_version: System::os_version().ok(),
        ..EnvSnapshot::default()
    };
    for (service, state) in app.state::<AppState>().call_capture.permission_states() {
        snapshot
            .permissions
            .insert(service.as_str(), state.as_str());
    }
    match browser_shortcut::readiness() {
        browser_shortcut::Readiness::Ready => {
            snapshot.permissions.insert("accessibility", "granted");
        }
        browser_shortcut::Readiness::AccessibilityMissing => {
            snapshot.permissions.insert("accessibility", "denied");
        }
        browser_shortcut::Readiness::Unsupported => {}
    }
    snapshot
}
