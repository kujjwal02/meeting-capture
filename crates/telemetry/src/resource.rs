//! The OpenTelemetry resource: which program, version, process and machine wrote the lines.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest as _, Sha256};

/// Each Rust program that logs. The name starts its log files and recorder dumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// The tray app.
    App,
    /// The tray app's binary started by Chrome for native messaging. Several run at once, one
    /// per Chrome profile.
    NativeHost,
    /// The audio and video helper. An audio and a video process can run at once.
    McCapture,
    /// The CLI.
    Mc,
}

impl Service {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::NativeHost => "native-host",
            Self::McCapture => "mc-capture",
            Self::Mc => "mc",
        }
    }

    /// `recorder-<service>.jsonl`, the file a dump of this service's recorder goes to.
    #[must_use]
    pub fn recorder_file_name(self) -> String {
        format!("recorder-{}.jsonl", self.name())
    }
}

#[cfg(target_os = "macos")]
const OS_TYPE: &str = "darwin";
#[cfg(not(target_os = "macos"))]
const OS_TYPE: &str = std::env::consts::OS;

#[cfg(target_arch = "aarch64")]
const HOST_ARCH: &str = "arm64";
#[cfg(target_arch = "x86_64")]
const HOST_ARCH: &str = "amd64";
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
const HOST_ARCH: &str = std::env::consts::ARCH;

/// Written once at the top of each log file a process opens, and in each recorder dump.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Resource {
    #[serde(rename = "service.name")]
    pub service_name: &'static str,
    #[serde(rename = "service.version")]
    pub service_version: &'static str,
    #[serde(rename = "service.instance.id")]
    pub instance_id: String,
    #[serde(rename = "process.pid")]
    pub pid: u32,
    #[serde(rename = "os.type")]
    pub os_type: &'static str,
    #[serde(rename = "host.arch")]
    pub host_arch: &'static str,
}

impl Resource {
    /// The resource of this process. The instance id is new on every start.
    pub(crate) fn of_this_process(service: Service, service_version: &'static str) -> Self {
        let pid = std::process::id();
        let started_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let mut hasher = Sha256::new();
        hasher.update(service.name().as_bytes());
        hasher.update(pid.to_le_bytes());
        hasher.update(started_ns.to_le_bytes());
        let digest = hasher.finalize();
        let instance_id = digest[..16].iter().fold(String::new(), |mut hex, byte| {
            use std::fmt::Write as _;
            let _infallible = write!(hex, "{byte:02x}");
            hex
        });
        Self {
            service_name: service.name(),
            service_version,
            instance_id,
            pid,
            os_type: OS_TYPE,
            host_arch: HOST_ARCH,
        }
    }

    /// The seed for this process's span ids, from its instance id.
    pub(crate) fn span_id_seed(&self) -> u64 {
        u64::from_str_radix(&self.instance_id[..16], 16).unwrap_or(1)
    }
}
