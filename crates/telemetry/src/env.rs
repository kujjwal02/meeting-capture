//! Environment rows: the machine, its audio devices, permissions and versions, at app start, at a
//! call start and at each change. A call's rows go to `env.jsonl` in its debug folder, and every
//! row also goes to `logs/env-<UTC date>.jsonl`, kept 14 days.
//!
//! The probes stay in the crates that own the OS calls. They fill an [`EnvSnapshot`]; this module
//! adds the resource fields and writes the row. A field a probe cannot fill is absent, not null.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};
use serde::Serialize;

use crate::line::encode;

/// The file name in a call's debug folder.
pub const ENV_FILE_NAME: &str = "env.jsonl";
/// Starts the name of the daily files in the log folder, `env-<UTC date>.jsonl`.
pub const DAILY_FILE_PREFIX: &str = "env";
const DAILY_RETENTION_DAYS: i32 = 14;

/// Why a row was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvReason {
    AppStart,
    CallStart,
    /// A device, route or permission changed.
    Change,
}

/// What the probes found. Each crate that owns an OS call fills its part.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EnvSnapshot {
    #[serde(rename = "os.version", skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    #[serde(
        rename = "mc.audio.input.default",
        skip_serializing_if = "Option::is_none"
    )]
    pub audio_input_default: Option<AudioDevice>,
    #[serde(
        rename = "mc.audio.output.default",
        skip_serializing_if = "Option::is_none"
    )]
    pub audio_output_default: Option<AudioDevice>,
    #[serde(
        rename = "mc.audio.mic.chosen",
        skip_serializing_if = "Option::is_none"
    )]
    pub mic_chosen: Option<ChosenMic>,
    /// Permission name to state, both from the closed types of the crate that reads them.
    #[serde(rename = "mc.permissions", skip_serializing_if = "BTreeMap::is_empty")]
    pub permissions: BTreeMap<&'static str, &'static str>,
    #[serde(rename = "mc.power", skip_serializing_if = "Option::is_none")]
    pub power: Option<PowerState>,
    #[serde(rename = "mc.displays", skip_serializing_if = "Option::is_none")]
    pub display_count: Option<u32>,
    #[serde(rename = "mc.browser", skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserVersions>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AudioDevice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The transport's name from the audio crate's closed type, such as `bluetooth`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<&'static str>,
}

/// The input the mic rule chose and why.
#[derive(Debug, Clone, Serialize)]
pub struct ChosenMic {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The reason's name from the audio crate's closed type, such as `default_is_bluetooth`.
    pub reason: &'static str,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PowerState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lid: Option<&'static str>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BrowserVersions {
    #[serde(rename = "chrome.version", skip_serializing_if = "Option::is_none")]
    pub chrome_version: Option<String>,
    #[serde(rename = "mc_meet.version", skip_serializing_if = "Option::is_none")]
    pub mc_meet_version: Option<String>,
    #[serde(rename = "meet.build", skip_serializing_if = "Option::is_none")]
    pub meet_build: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum EnvRecord {
    Env,
}

#[derive(Serialize)]
struct EnvRow<'a> {
    record: EnvRecord,
    timestamp: Timestamp,
    reason: EnvReason,
    #[serde(rename = "service.name", skip_serializing_if = "Option::is_none")]
    service_name: Option<&'static str>,
    #[serde(
        rename = "service.instance.id",
        skip_serializing_if = "Option::is_none"
    )]
    instance_id: Option<&'a str>,
    #[serde(rename = "service.version", skip_serializing_if = "Option::is_none")]
    service_version: Option<&'static str>,
    #[serde(rename = "os.type", skip_serializing_if = "Option::is_none")]
    os_type: Option<&'static str>,
    #[serde(rename = "host.arch", skip_serializing_if = "Option::is_none")]
    host_arch: Option<&'static str>,
    #[serde(flatten)]
    snapshot: &'a EnvSnapshot,
}

impl EnvSnapshot {
    /// Appends the row to the day's env file, and to `env.jsonl` in `call_folder` when the row
    /// belongs to a call.
    ///
    /// # Errors
    ///
    /// Returns the first write that failed. The other file is still tried.
    pub fn write(
        &self,
        reason: EnvReason,
        log_dir: &Path,
        call_folder: Option<&Path>,
    ) -> std::io::Result<()> {
        let at = Timestamp::now();
        let resource = crate::init::installed_resource();
        let row = EnvRow {
            record: EnvRecord::Env,
            timestamp: at,
            reason,
            service_name: resource.as_ref().map(|resource| resource.service_name),
            instance_id: resource
                .as_ref()
                .map(|resource| resource.instance_id.as_str()),
            service_version: resource.as_ref().map(|resource| resource.service_version),
            os_type: resource.as_ref().map(|resource| resource.os_type),
            host_arch: resource.as_ref().map(|resource| resource.host_arch),
            snapshot: self,
        };
        let Some(line) = encode(&row) else {
            return Ok(());
        };
        let in_folder = call_folder.map_or(Ok(()), |folder| {
            std::fs::create_dir_all(folder)?;
            append(&folder.join(ENV_FILE_NAME), &line)
        });
        let daily = append_daily(log_dir, DAILY_FILE_PREFIX, at, &line);
        in_folder.and(daily)
    }
}

fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    // One write on a file opened for append, so rows from two writers never interleave.
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line)
}

/// Appends a line to `<dir>/<prefix>-<UTC date>.jsonl`. The day's first line deletes the
/// prefix's files older than 14 days.
pub(crate) fn append_daily(
    dir: &Path,
    prefix: &str,
    at: Timestamp,
    line: &[u8],
) -> std::io::Result<()> {
    let date = at.to_zoned(TimeZone::UTC).date();
    let path = dir.join(daily_file_name(prefix, date));
    let is_new_day = !path.exists();
    std::fs::create_dir_all(dir)?;
    append(&path, line)?;
    if is_new_day {
        remove_expired_daily(dir, prefix, date);
    }
    Ok(())
}

fn daily_file_name(prefix: &str, date: Date) -> String {
    format!("{prefix}-{date}.jsonl")
}

fn remove_expired_daily(dir: &Path, prefix: &str, today: Date) {
    let Ok(cutoff) = today.checked_sub(DAILY_RETENTION_DAYS.days()) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let is_expired = name
            .to_string_lossy()
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('-'))
            .and_then(|rest| rest.strip_suffix(".jsonl"))
            .and_then(|date| date.parse::<Date>().ok())
            .is_some_and(|date| date < cutoff);
        if !is_expired {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(file.path = %path.display(), "removed an old daily file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                file.path = %path.display(),
                exception.message = %error,
                "could not remove an old daily file"
            ),
        }
    }
}
