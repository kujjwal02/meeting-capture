//! Debug material for calls that go wrong: frames mc-meet could not decode, the mc-meet events
//! before the first problem, page snapshots, flight recorder dumps, and the debug report the user
//! saves. It all stays in the app data folder: raw frames and events hold call speech.

mod redaction;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use jiff::Timestamp;
use pipeline::settings::AppDirs;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager as _};

use self::redaction::Redactor;
use crate::AppState;
use crate::call_status::Moment;
use crate::calls::CallPlatform;
use crate::launch::{LaunchTarget, launch};

const MAX_FAILED_FRAMES: u32 = 20;
const LEAD_UP_SPAN: Duration = Duration::from_mins(2);
const MAX_PAGE_SNAPSHOTS: u32 = 5;
const RETENTION: Duration = Duration::from_hours(14 * 24);
const RETENTION_CHECK_INTERVAL: Duration = Duration::from_hours(24);
/// What the debug folders and reports may hold together. The retention pass deletes the oldest
/// folders first until they fit.
const TOTAL_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// A call's folder size is read again this often while its files grow.
const FOLDER_MEASURE_INTERVAL: Duration = Duration::from_secs(10);
const FRAMES_FILE: &str = "frames.jsonl";
const MEET_EVENTS_FILE: &str = "events.jsonl";
const LEAD_UP_FILE: &str = "lead-up.jsonl";
const PAGE_SNAPSHOTS_FILE: &str = "page-snapshots.jsonl";
const CALL_STATUS_FILE: &str = "call-status.json";
const APP_LOG_FILE: &str = "app-log.jsonl";
/// Starts the name of each flight recorder dump in a call's folder.
const RECORDER_FILE_PREFIX: &str = "recorder-";
const JOB_FILE: &str = "job.json";
/// The capture helpers' manifests and the finalize record in the call folder. `finalize.json`
/// is spelled here: its constant lives in `call-finalize`, which the app does not link.
const CAPTURE_RECORD_FILES: [&str; 3] = [
    stem_writer::MANIFEST_FILE_NAME,
    video_capture::MANIFEST_FILE_NAME,
    "finalize.json",
];
/// Log lines this long before a call's start or after its end still belong to it: a helper
/// starts before the first caption and closes its files after the call ends.
const CALL_WINDOW_MARGIN: jiff::SignedDuration = jiff::SignedDuration::from_mins(1);
/// What one copied file of a report may hold. A day's log at DEBUG can pass it; the cap keeps a
/// report small enough to send.
const REPORT_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// In `call-status.json`: the report's files that reached the cap and end early.
const TRUNCATED_FILES_KEY: &str = "mc.report.truncated_files";
/// What "the last 30 minutes" report covers.
const RECENT_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_mins(30);
/// The services whose daily log files a "last 30 minutes" report takes.
const ALL_SERVICES: [telemetry::Service; 4] = [
    telemetry::Service::App,
    telemetry::Service::NativeHost,
    telemetry::Service::McCapture,
    telemetry::Service::Mc,
];

/// The caption channel a frame came from. The names are Meet's channel labels. mc-meet sends no
/// failures of its other channels: a chat or roster frame with nothing to read is common.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FrameChannel {
    Captions,
    CaptionsV2,
}

impl FrameChannel {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Captions => "captions",
            Self::CaptionsV2 => "captions_v2",
        }
    }
}

/// A data channel frame mc-meet could not decode, as it sent it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FailedFrame {
    pub channel: FrameChannel,
    /// When the frame arrived in the page.
    pub at: Timestamp,
    /// The decoder's reason, such as `no-device`.
    pub reason: String,
    /// The tag and wire type tree with lengths, no text.
    pub skeleton: String,
    pub byte_length: u64,
    /// The frame as received, before gzip inflation. `None` for a frame too large to send.
    #[serde(default)]
    pub bytes_base64: Option<String>,
}

/// Debug files per call, and what each call wrote so far in this run.
pub(crate) struct CallDebug {
    call_folders_dir: PathBuf,
    report_dir: PathBuf,
    log_dir: PathBuf,
    calls_dir: PathBuf,
    tallies: Mutex<HashMap<PathBuf, DebugTally>>,
}

#[derive(Default)]
struct DebugTally {
    lead_up: VecDeque<(Instant, serde_json::Value)>,
    is_lead_up_written: bool,
    failed_frames: u32,
    page_snapshots: u32,
    folder_measured_at: Option<Instant>,
    /// The folder reached `telemetry::metrics::DEBUG_FOLDER_MAX_BYTES`: nothing more is appended.
    is_folder_full: bool,
}

/// What a debug report covers.
pub(crate) struct ReportRequest {
    pub call_id: PathBuf,
    /// The call's state from the status board, when the board still has the call.
    pub call_status: Option<serde_json::Value>,
    /// Session ids whose app log lines belong to the call.
    pub session_ids: Vec<String>,
    /// Keep caption and chat text. Without it, each text is replaced by its length, and frame
    /// bytes are left out.
    pub is_with_text: bool,
    /// RFC 3339 UTC while debug mode is on.
    pub debug_mode_until: Option<String>,
    /// The job store's row for the meeting made from the call, once the pipeline took it.
    pub job: Option<JobRecord>,
}

/// One meeting's row in the job store, as the report writes it.
pub(crate) struct JobRecord {
    pub meeting_id: jobs::MeetingId,
    /// The meeting's input revision now. Each revision's run had a trace of its own.
    pub input_revision: i64,
    pub row: serde_json::Value,
}

/// The span of time whose log lines a report keeps.
#[derive(Clone, Copy)]
struct TimeWindow {
    start: Timestamp,
    end: Timestamp,
}

impl TimeWindow {
    /// Whether the line's `timestamp` falls in the window. A line without one is left out. A
    /// `resource` line is always kept: a process that started before the window still names
    /// the process of the lines in it.
    fn holds_line(self, line: &str) -> bool {
        let facts = LineFacts::of(line);
        facts.record == Some(LineRecord::Resource) || self.holds(facts.at)
    }

    fn holds(self, at: Option<Timestamp>) -> bool {
        at.is_some_and(|at| self.start <= at && at <= self.end)
    }

    /// The daily files, named `<prefix>-<UTC date>.jsonl`, of the UTC days the window touches.
    /// A file whose name holds no date is kept.
    fn files_of(self, files: Vec<PathBuf>) -> Vec<PathBuf> {
        let first_day = self.start.to_zoned(jiff::tz::TimeZone::UTC).date();
        let last_day = self.end.to_zoned(jiff::tz::TimeZone::UTC).date();
        files
            .into_iter()
            .filter(|path| file_day(path).is_none_or(|day| first_day <= day && day <= last_day))
            .collect()
    }
}

// The UTC date that ends a daily file's name, as in `app-2026-11-01.jsonl`.
fn file_day(path: &Path) -> Option<jiff::civil::Date> {
    let stem = path.file_stem()?.to_str()?;
    let date_start = stem.len().checked_sub("YYYY-MM-DD".len())?;
    stem.get(date_start..)?.parse().ok()
}

impl CallDebug {
    pub(crate) fn new(app_dirs: &AppDirs) -> Self {
        Self {
            call_folders_dir: app_dirs.call_debug_dir(),
            report_dir: app_dirs.debug_report_dir(),
            log_dir: app_dirs.log_dir(),
            calls_dir: app_dirs.calls_dir(),
            tallies: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, DebugTally>> {
        self.tallies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The debug folder of the call whose log is `call_id`.
    pub(crate) fn call_folder(&self, call_id: &Path) -> PathBuf {
        self.call_folders_dir.join(call_stem(call_id))
    }

    /// Dumps the app's flight recorder into the call's debug folder. A failure is only logged:
    /// the dump is evidence, never a reason to stop.
    pub(crate) fn dump_recorder(&self, call_id: &Path, reason: telemetry::DumpReason) {
        let folder = self.call_folder(call_id);
        if let Err(error) = telemetry::dump_recorder_into(&folder, reason) {
            tracing::warn!(
                call.id = %call_id.display(),
                exception.message = %error,
                "could not dump the flight recorder"
            );
        }
    }

    /// Where a capture helper dumps its recorder for the call: one file per helper kind, as an
    /// audio and a video helper run at once.
    pub(crate) fn helper_recorder_path(&self, call_id: &Path, helper_kind: &str) -> PathBuf {
        self.call_folder(call_id)
            .join(format!("recorder-mc-capture-{helper_kind}.jsonl"))
    }

    /// Keeps one mc-meet message for the lead-up, and drops what is older than the span.
    pub(crate) fn remember(&self, call_id: &Path, mut message: serde_json::Value) {
        if let Some(fields) = message.as_object_mut() {
            fields.insert("received_at".to_owned(), received_at());
        }
        let now = Instant::now();
        let mut tallies = self.lock();
        let lead_up = &mut tallies.entry(call_id.to_path_buf()).or_default().lead_up;
        lead_up.push_back((now, message));
        while lead_up
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > LEAD_UP_SPAN)
        {
            lead_up.pop_front();
        }
    }

    /// Appends an mc-meet event line to the call's `events.jsonl`, so the debug folder alone shows
    /// the presses and channel changes next to the counters in `metrics.jsonl`.
    pub(crate) fn record_meet_event(&self, call_id: &Path, line: &serde_json::Value) {
        self.append_line(call_id, MEET_EVENTS_FILE, line);
    }

    /// Writes the frame to the call's `frames.jsonl`, for the first `MAX_FAILED_FRAMES` of the
    /// call in this run.
    pub(crate) fn record_failed_frame(&self, call_id: &Path, frame: &FailedFrame) {
        let summary = serde_json::json!({
            "type": "frame_failed",
            "channel": frame.channel,
            "at": frame.at,
            "reason": frame.reason,
            "skeleton": frame.skeleton,
            "byte_length": frame.byte_length,
        });
        self.remember(call_id, summary);
        let is_kept = {
            let mut tallies = self.lock();
            let tally = tallies.entry(call_id.to_path_buf()).or_default();
            tally.failed_frames += 1;
            tally.failed_frames <= MAX_FAILED_FRAMES
        };
        if !is_kept {
            return;
        }
        let line = match serde_json::to_value(frame) {
            Ok(mut line) => {
                if let Some(fields) = line.as_object_mut() {
                    fields.insert("received_at".to_owned(), received_at());
                }
                line
            }
            Err(error) => {
                tracing::warn!(exception.message = %error, "could not encode a failed frame");
                return;
            }
        };
        self.append_line(call_id, FRAMES_FILE, &line);
    }

    /// Whether a page snapshot may be asked for: the first `MAX_PAGE_SNAPSHOTS` per call.
    pub(crate) fn take_snapshot_turn(&self, call_id: &Path) -> bool {
        let mut tallies = self.lock();
        let tally = tallies.entry(call_id.to_path_buf()).or_default();
        tally.page_snapshots += 1;
        tally.page_snapshots <= MAX_PAGE_SNAPSHOTS
    }

    pub(crate) fn record_page_snapshot(
        &self,
        call_id: &Path,
        session_id: &str,
        snapshot: &serde_json::Value,
    ) {
        let line = serde_json::json!({
            "received_at": received_at(),
            "session_id": session_id,
            "snapshot": snapshot,
        });
        self.append_line(call_id, PAGE_SNAPSHOTS_FILE, &line);
    }

    /// Writes the call's lead-up to its debug folder, once per call.
    pub(crate) fn write_lead_up(&self, call_id: &Path) {
        let lines: Vec<serde_json::Value> = {
            let mut tallies = self.lock();
            let tally = tallies.entry(call_id.to_path_buf()).or_default();
            if std::mem::replace(&mut tally.is_lead_up_written, true) {
                return;
            }
            tally
                .lead_up
                .iter()
                .map(|(_, message)| message.clone())
                .collect()
        };
        for line in &lines {
            self.append_line(call_id, LEAD_UP_FILE, line);
        }
        tracing::info!(
            call.id = %call_id.display(),
            file.directory = %self.call_folder(call_id).display(),
            "wrote the lead-up to a call problem"
        );
    }

    /// Forgets a call that left the recent calls list. A later resume starts new caps.
    pub(crate) fn forget_call(&self, call_id: &Path) {
        self.lock().remove(call_id);
    }

    /// Drops the call's lead-up once it ended. Its counts stay, so a resumed call does not write
    /// past its caps.
    pub(crate) fn forget_lead_up(&self, call_id: &Path) {
        if let Some(tally) = self.lock().get_mut(call_id) {
            tally.lead_up.clear();
        }
    }

    /// Whether the call's debug folder is under its size cap. Logs once when it reaches it.
    pub(crate) fn has_room(&self, call_id: &Path) -> bool {
        let folder = self.call_folder(call_id);
        let mut tallies = self.lock();
        let tally = tallies.entry(call_id.to_path_buf()).or_default();
        if tally.is_folder_full {
            return false;
        }
        let now = Instant::now();
        if tally
            .folder_measured_at
            .is_some_and(|at| now.duration_since(at) < FOLDER_MEASURE_INTERVAL)
        {
            return true;
        }
        tally.folder_measured_at = Some(now);
        let max_bytes = telemetry::metrics::DEBUG_FOLDER_MAX_BYTES;
        if telemetry::metrics::folder_size_bytes(&folder) < max_bytes {
            return true;
        }
        tally.is_folder_full = true;
        tracing::warn!(
            call.id = %call_id.display(),
            file.directory = %folder.display(),
            mc.debug.folder_max_bytes = max_bytes,
            "the call's debug folder reached its size cap; its frames and snapshots stop"
        );
        false
    }

    fn append_line(&self, call_id: &Path, file_name: &str, line: &serde_json::Value) {
        if !self.has_room(call_id) {
            return;
        }
        let folder = self.call_folder(call_id);
        let path = folder.join(file_name);
        let appended = std::fs::create_dir_all(&folder).and_then(|()| {
            let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
            let mut text = serde_json::to_vec(line).map_err(std::io::Error::other)?;
            text.push(b'\n');
            file.write_all(&text)
        });
        if let Err(error) = appended {
            tracing::warn!(
                file.path = %path.display(),
                exception.message = %error,
                "could not write a call debug file"
            );
        }
    }

    /// Deletes debug folders and reports older than 14 days: call folders, pipeline run folders
    /// and panic folders, with the recorder dumps in them. Then deletes the oldest folders until
    /// all of them hold 1 GB or less.
    pub(crate) fn remove_expired(&self) {
        self.remove_old();
        self.remove_over_total();
    }

    fn remove_over_total(&self) {
        let mut folders: Vec<(SystemTime, u64, PathBuf)> =
            [&self.call_folders_dir, &self.report_dir]
                .into_iter()
                .filter_map(|dir| std::fs::read_dir(dir).ok())
                .flat_map(|entries| entries.filter_map(Result::ok))
                .filter(|entry| entry.file_type().is_ok_and(|file_type| file_type.is_dir()))
                .map(|entry| {
                    let path = entry.path();
                    let modified = entry
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    (modified, telemetry::metrics::folder_size_bytes(&path), path)
                })
                .collect();
        let mut total_bytes = folders
            .iter()
            .fold(0_u64, |total, (_, bytes, _)| total.saturating_add(*bytes));
        folders.sort();
        for (_, bytes, path) in folders {
            if total_bytes <= TOTAL_MAX_BYTES {
                break;
            }
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    total_bytes = total_bytes.saturating_sub(bytes);
                    tracing::info!(
                        file.directory = %path.display(),
                        mc.debug.total_max_bytes = TOTAL_MAX_BYTES,
                        "removed the oldest debug folder over the total cap"
                    );
                }
                Err(error) => tracing::warn!(
                    file.directory = %path.display(),
                    exception.message = %error,
                    "could not remove a debug folder over the total cap"
                ),
            }
        }
    }

    fn remove_old(&self) {
        let Some(cutoff) = SystemTime::now().checked_sub(RETENTION) else {
            return;
        };
        for dir in [&self.call_folders_dir, &self.report_dir] {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(
                        file.directory = %dir.display(),
                        exception.message = %error,
                        "could not list debug folders"
                    );
                    continue;
                }
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                let is_expired = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .is_ok_and(|modified| modified < cutoff);
                if !is_expired || !path.is_dir() {
                    continue;
                }
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => {
                        tracing::info!(file.directory = %path.display(), "removed an old debug folder");
                    }
                    Err(error) => {
                        tracing::warn!(
                            file.directory = %path.display(),
                            exception.message = %error,
                            "could not remove an old debug folder"
                        );
                    }
                }
            }
        }
    }

    /// Gathers the call's evidence into one new folder and returns the folder: the app log lines
    /// of the call, the helper's log lines in the call's time span, the call log, the capture
    /// manifests, the job row and every file of the call's and the meeting's debug folders.
    pub(crate) fn save_report(&self, request: &ReportRequest) -> std::io::Result<PathBuf> {
        let stem = call_stem(&request.call_id);
        let saved_at = Timestamp::now();
        let (folder, mut output) = self.new_report_folder(&stem, saved_at, request.is_with_text)?;

        // As the log writes them: JSON escapes the backslashes of a Windows path.
        let call_id_text = request.call_id.to_string_lossy().into_owned();
        let call_trace_id = telemetry::trace::TraceId::for_call(&call_id_text).to_string();
        let needles: Vec<String> = [call_id_text.clone(), call_trace_id.clone()]
            .into_iter()
            .chain(request.session_ids.iter().cloned())
            .map(|needle| json_escaped(&needle))
            .collect();
        let app_logs = telemetry::daily_log_files(&self.log_dir, telemetry::Service::App)?;
        output.copy_json_lines(&app_logs, &folder.join(APP_LOG_FILE), |line| {
            needles.iter().any(|needle| line.contains(needle.as_str()))
        })?;

        // The helper's lines carry the call only inside its segment span. A helper that has
        // such a line belongs to the call; its lines outside any trace, such as its start and
        // stop, come along within the call's time span.
        let window = call_window(request, saved_at);
        let helper_logs = telemetry::daily_log_files(&self.log_dir, telemetry::Service::McCapture)?;
        let call = CallMatch {
            call_id: call_id_text,
            trace_id: call_trace_id,
        };
        output.copy_process_lines(
            &window.files_of(helper_logs),
            &folder.join(service_log_file(telemetry::Service::McCapture)),
            window,
            Some(&call),
        )?;

        if let Some(file_name) = request.call_id.file_name() {
            output.copy_json_lines(
                std::slice::from_ref(&request.call_id),
                &folder.join(file_name),
                |_| true,
            )?;
        }

        let capture_folder = self.calls_dir.join(&stem);
        for file_name in CAPTURE_RECORD_FILES {
            output.copy_json_document(&capture_folder.join(file_name), &folder.join(file_name))?;
        }

        if let Some(job) = &request.job {
            output.write_json_document(&folder.join(JOB_FILE), job.row.clone())?;
            let meeting_folder = self
                .call_folders_dir
                .join(format!("meeting-{}", job.meeting_id));
            // A failed run's dump holds every line the recorder kept, other calls' too.
            let meeting_lines = RecorderLines::of_meeting(job.meeting_id, job.input_revision);
            output.copy_debug_folder(
                &meeting_folder,
                &folder.join(format!("debug-meeting-{}", job.meeting_id)),
                |line| meeting_lines.holds(line),
            )?;
        }

        // Into the call's folder, so the copy below redacts it like every other debug file.
        self.dump_recorder(&request.call_id, telemetry::DumpReason::DebugReport);
        // A recorder holds every call's lines and the pipeline's: a report is about one call.
        let call_lines = RecorderLines::of_call(&request.call_id);
        output.copy_debug_folder(
            &self.call_folder(&request.call_id),
            &folder.join("debug"),
            |line| call_lines.holds(line),
        )?;

        let status = serde_json::json!({
            "saved_at": saved_at,
            "is_with_text": request.is_with_text,
            "app_version": env!("CARGO_PKG_VERSION"),
            "debug_mode_until": request.debug_mode_until,
            "call": request.call_status,
        });
        output.write_status(&folder, status)?;
        tracing::info!(
            call.id = %request.call_id.display(),
            file.directory = %folder.display(),
            "saved a debug report"
        );
        Ok(folder)
    }

    /// Gathers every service's log lines of the last 30 minutes, the environment and host
    /// metrics lines and a dump of the app's flight recorder into one new folder, and returns
    /// the folder. It needs no call: for a bug outside one, or a call the app never saw.
    pub(crate) fn save_recent_report(&self, is_with_text: bool) -> std::io::Result<PathBuf> {
        let saved_at = Timestamp::now();
        let window = TimeWindow {
            start: saved_at - RECENT_WINDOW,
            end: saved_at,
        };
        let (folder, mut output) = self.new_report_folder("last-30-min", saved_at, is_with_text)?;

        for service in ALL_SERVICES {
            let logs = telemetry::daily_log_files(&self.log_dir, service)?;
            output.copy_process_lines(
                &window.files_of(logs),
                &folder.join(service_log_file(service)),
                window,
                None,
            )?;
        }
        for (prefix, file_name) in [
            (
                telemetry::env::DAILY_FILE_PREFIX,
                telemetry::env::ENV_FILE_NAME,
            ),
            (
                telemetry::metrics::DAILY_FILE_PREFIX,
                telemetry::metrics::METRICS_FILE_NAME,
            ),
        ] {
            output.copy_json_lines(
                &window.files_of(daily_row_files(&self.log_dir, prefix)?),
                &folder.join(file_name),
                |line| window.holds_line(line),
            )?;
        }

        // A folder of its own under `debug/`, so the copy redacts the dump and the retention
        // sweep removes it like any other debug folder.
        let dump_folder = self
            .call_folders_dir
            .join(format!("recent-{}", saved_at.strftime("%Y%m%dT%H%M%SZ")));
        if let Err(error) =
            telemetry::dump_recorder_into(&dump_folder, telemetry::DumpReason::DebugReport)
        {
            tracing::warn!(exception.message = %error, "could not dump the flight recorder");
        }
        output.copy_debug_folder(&dump_folder, &folder.join("debug"), |_| true)?;

        let status = serde_json::json!({
            "saved_at": saved_at,
            "is_with_text": is_with_text,
            "app_version": env!("CARGO_PKG_VERSION"),
            "started_at": window.start,
            "ended_at": window.end,
        });
        output.write_status(&folder, status)?;
        tracing::info!(file.directory = %folder.display(), "saved a debug report of the last 30 min");
        Ok(folder)
    }

    fn new_report_folder(
        &self,
        name: &str,
        saved_at: Timestamp,
        is_with_text: bool,
    ) -> std::io::Result<(PathBuf, ReportOutput)> {
        let suffix = if is_with_text { "-with-text" } else { "" };
        let folder = self.report_dir.join(format!(
            "{}-{name}{suffix}",
            saved_at.strftime("%Y%m%dT%H%M%SZ")
        ));
        std::fs::create_dir_all(&folder)?;
        let redaction = if is_with_text {
            Redaction::KeepText
        } else {
            Redaction::TextLengths(Redactor::default())
        };
        Ok((
            folder,
            ReportOutput {
                redaction,
                truncated_files: Vec::new(),
            },
        ))
    }
}

/// Dumps the app's flight recorder into the call's debug folder on a blocking thread: a dump
/// writes up to 8 MB.
pub(crate) fn dump_recorder_in_background(
    app: &AppHandle,
    call_id: PathBuf,
    reason: telemetry::DumpReason,
) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        app.state::<AppState>()
            .call_debug
            .dump_recorder(&call_id, reason);
    });
}

/// Deletes old debug folders now and once a day.
pub(crate) fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(RETENTION_CHECK_INTERVAL);
        loop {
            interval.tick().await;
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                app.state::<AppState>().call_debug.remove_expired();
            });
        }
    });
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReportError {
    #[error("no call to report on since the app started")]
    NoCall,
    #[error("could not write the debug report: {0}")]
    Write(#[from] std::io::Error),
    #[error(transparent)]
    InvalidSessionId(#[from] crate::browser_calls::InvalidSessionId),
}

/// Saves a debug report for the call of `session_id`, else the current or last call, and shows
/// it in Finder or Explorer.
pub(crate) fn save_debug_report(
    app: &AppHandle,
    session_id: Option<&str>,
    is_with_text: bool,
) -> Result<PathBuf, ReportError> {
    let state = app.state::<AppState>();
    let now = Moment::now();
    let board = state.calls.status();
    let session_call = match session_id {
        Some(session_id) => state.browser_calls.call_of_session(session_id)?,
        None => None,
    };
    let (call_id, platform) = session_call
        .map(|call_id| (call_id, CallPlatform::Meet))
        .or_else(|| board.view(now).current_or_last_call())
        .ok_or(ReportError::NoCall)?;
    let session_ids = match platform {
        CallPlatform::Meet => state.browser_calls.sessions_of_call(&call_id),
        CallPlatform::SlackHuddle => Vec::new(),
    };
    let job = state
        .pipeline
        .as_ref()
        .and_then(|pipeline| job_record(pipeline, &call_id));
    let request = ReportRequest {
        call_status: board.describe(&call_id, now),
        session_ids,
        call_id,
        is_with_text,
        debug_mode_until: state.debug_mode.mode().at(Timestamp::now()).enabled_until(),
        job,
    };
    let folder = state.call_debug.save_report(&request)?;
    launch(app, &LaunchTarget::RevealFile(&folder));
    Ok(folder)
}

/// Saves a debug report of the last 30 minutes and shows it in Finder or Explorer.
pub(crate) fn save_recent_debug_report(
    app: &AppHandle,
    is_with_text: bool,
) -> Result<PathBuf, ReportError> {
    let folder = app
        .state::<AppState>()
        .call_debug
        .save_recent_report(is_with_text)?;
    launch(app, &LaunchTarget::RevealFile(&folder));
    Ok(folder)
}

/// How many recent meetings the report searches for the call's: a call ends up among the newest.
const RECENT_MEETINGS_SEARCHED: u32 = 50;

/// The job row of the meeting made from the call log `call_id`, if the pipeline took it. A
/// failed lookup is only logged: the report is still worth saving without it.
fn job_record(pipeline: &pipeline::Pipeline, call_id: &Path) -> Option<JobRecord> {
    let meetings = match pipeline.store().recent(RECENT_MEETINGS_SEARCHED) {
        Ok(meetings) => meetings,
        Err(error) => {
            tracing::warn!(exception.message = %error, "could not read the job store for a report");
            return None;
        }
    };
    let meeting = meetings
        .into_iter()
        .find(|meeting| meeting.input_path == call_id)?;
    let state = meeting.state();
    let failure = match &state {
        jobs::JobState::Failed { cause } => Some(cause.message.clone()),
        jobs::JobState::Captured
        | jobs::JobState::Matched
        | jobs::JobState::Extracted
        | jobs::JobState::Written { .. }
        | jobs::JobState::ExtractionPending { .. } => None,
    };
    // The note path names the meeting, so it goes under a key the redaction shortens.
    let row = serde_json::json!({
        "meeting.id": meeting.id.as_i64(),
        "mc.input.revision": meeting.input_revision,
        "started_at": meeting.started_at,
        "title": meeting.title,
        "job.step": meeting.finished_step.as_str(),
        "state": state.label(),
        "exception.message": failure,
        "retry_count": meeting.retry_count,
        "extraction_provider": meeting.extraction_provider,
        "note.path": meeting.note.path().map(|path| path.to_string_lossy().into_owned()),
        "created_at": meeting.created_at,
        "updated_at": meeting.updated_at,
    });
    Some(JobRecord {
        meeting_id: meeting.id,
        input_revision: meeting.input_revision,
        row,
    })
}

enum Redaction {
    KeepText,
    /// Only times, ids, versions and codes stay. Other text becomes its length, and frame bytes
    /// are left out.
    TextLengths(Redactor),
}

/// Writes a report's files: every one through the redaction, and none past
/// `REPORT_FILE_MAX_BYTES`.
struct ReportOutput {
    redaction: Redaction,
    /// The report's files that reached the cap, by file name.
    truncated_files: Vec<String>,
}

impl ReportOutput {
    // Copies the lines of each file in `from` that `is_wanted` keeps, in order, into `to`. A
    // missing file copies nothing: an ended huddle may have no debug folder, and a call log may
    // have been set aside. `to` is written only when a file in `from` exists.
    fn copy_json_lines(
        &mut self,
        from: &[PathBuf],
        to: &Path,
        is_wanted: impl Fn(&str) -> bool,
    ) -> std::io::Result<()> {
        let mut target: Option<CappedFile> = None;
        for source_path in from {
            let source = match File::open(source_path) {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let target = match &mut target {
                Some(target) => target,
                None => target.insert(CappedFile::create(to)?),
            };
            for line in BufReader::new(source).lines() {
                let line = line?;
                if !is_wanted(&line) {
                    continue;
                }
                let redacted = match &mut self.redaction {
                    Redaction::KeepText => line,
                    Redaction::TextLengths(redactor) => {
                        let redacted = match serde_json::from_str::<serde_json::Value>(&line) {
                            Ok(mut value) => {
                                redactor.redact(&mut value);
                                value
                            }
                            Err(_) => serde_json::json!({
                                "unreadable_line_chars": line.chars().count()
                            }),
                        };
                        serde_json::to_string(&redacted).map_err(std::io::Error::other)?
                    }
                };
                if !target.write_line(&redacted)? {
                    break;
                }
            }
            if target.is_full {
                break;
            }
        }
        match target {
            Some(target) => {
                if target.finish()? {
                    self.note_truncated(to);
                }
                Ok(())
            }
            None => Ok(()),
        }
    }

    // Copies a service's log lines in `window`, and the `resource` line of each process that
    // has one there. With `call`, a process belongs only when it has a line of the call, and
    // a line of another trace is left out.
    fn copy_process_lines(
        &mut self,
        from: &[PathBuf],
        to: &Path,
        window: TimeWindow,
        call: Option<&CallMatch>,
    ) -> std::io::Result<()> {
        let mut processes: HashSet<String> = HashSet::new();
        for_each_line_facts(from, |facts| {
            if let Some(instance_id) = &facts.instance_id
                && facts.record != Some(LineRecord::Resource)
                && window.holds(facts.at)
                && call.is_none_or(|call| call.holds(&facts))
            {
                processes.insert(instance_id.clone());
            }
        })?;
        self.copy_json_lines(from, to, |line| {
            let facts = LineFacts::of(line);
            let is_process_kept = facts
                .instance_id
                .as_ref()
                .is_some_and(|instance_id| processes.contains(instance_id));
            if facts.record == Some(LineRecord::Resource) {
                return is_process_kept;
            }
            let is_call_line = match call {
                None => true,
                Some(call) => call.holds(&facts) || (facts.trace_id.is_none() && is_process_kept),
            };
            window.holds(facts.at) && is_call_line
        })
    }

    // Copies one JSON file through the redaction, pretty printed. A missing file copies
    // nothing: a call without video has no `video.json`.
    fn copy_json_document(&mut self, from: &Path, to: &Path) -> std::io::Result<()> {
        let text = match std::fs::read(from) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let document = serde_json::from_slice(&text)
            .unwrap_or_else(|_| serde_json::json!({ "unreadable_document_bytes": text.len() }));
        self.write_json_document(to, document)
    }

    fn write_json_document(
        &mut self,
        to: &Path,
        mut document: serde_json::Value,
    ) -> std::io::Result<()> {
        match &mut self.redaction {
            Redaction::KeepText => {}
            Redaction::TextLengths(redactor) => redactor.redact(&mut document),
        }
        std::fs::write(
            to,
            serde_json::to_vec_pretty(&document).map_err(std::io::Error::other)?,
        )
    }

    // Copies every file of a debug folder through the redaction: JSON documents whole, the
    // rest as JSON lines. Of a recorder dump it keeps only the lines `holds_recorder_line`
    // takes. A missing folder copies nothing.
    fn copy_debug_folder(
        &mut self,
        from: &Path,
        to: &Path,
        holds_recorder_line: impl Fn(&str) -> bool,
    ) -> std::io::Result<()> {
        if !from.is_dir() {
            return Ok(());
        }
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)?.filter_map(Result::ok) {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name_text = file_name.to_string_lossy();
            // A hidden or `.partial` file is a write that never finished.
            if !path.is_file()
                || file_name_text.starts_with('.')
                || file_name_text.ends_with(".partial")
            {
                continue;
            }
            let target = to.join(&file_name);
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                self.copy_json_document(&path, &target)?;
            } else if file_name_text.starts_with(RECORDER_FILE_PREFIX) {
                self.copy_json_lines(std::slice::from_ref(&path), &target, &holds_recorder_line)?;
            } else {
                self.copy_json_lines(std::slice::from_ref(&path), &target, |_| true)?;
            }
        }
        Ok(())
    }

    /// Writes `call-status.json` last, so it can name the files that reached the cap.
    fn write_status(
        &mut self,
        folder: &Path,
        mut status: serde_json::Value,
    ) -> std::io::Result<()> {
        if !self.truncated_files.is_empty()
            && let Some(fields) = status.as_object_mut()
        {
            fields.insert(
                TRUNCATED_FILES_KEY.to_owned(),
                serde_json::json!(self.truncated_files),
            );
            fields.insert(
                "mc.report.file_max_bytes".to_owned(),
                serde_json::json!(REPORT_FILE_MAX_BYTES),
            );
        }
        self.write_json_document(&folder.join(CALL_STATUS_FILE), status)
    }

    fn note_truncated(&mut self, path: &Path) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        tracing::warn!(
            file.path = %path.display(),
            mc.report.file_max_bytes = REPORT_FILE_MAX_BYTES,
            "a debug report file reached its size cap"
        );
        self.truncated_files.push(name);
    }
}

/// One report file, which stops taking lines at `REPORT_FILE_MAX_BYTES`.
struct CappedFile {
    writer: BufWriter<File>,
    written_bytes: u64,
    is_full: bool,
}

impl CappedFile {
    fn create(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            writer: BufWriter::new(File::create(path)?),
            written_bytes: 0,
            is_full: false,
        })
    }

    /// Writes the line and its newline. Returns false, and writes nothing, once the line would
    /// pass the cap.
    fn write_line(&mut self, line: &str) -> std::io::Result<bool> {
        let line_bytes = line.len() as u64 + 1;
        if self.is_full || self.written_bytes + line_bytes > REPORT_FILE_MAX_BYTES {
            self.is_full = true;
            return Ok(false);
        }
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.written_bytes += line_bytes;
        Ok(true)
    }

    /// Flushes the file. Returns whether it reached the cap.
    fn finish(mut self) -> std::io::Result<bool> {
        self.writer.flush()?;
        Ok(self.is_full)
    }
}

/// What the report's filters read of one telemetry line.
#[derive(Default)]
struct LineFacts {
    record: Option<LineRecord>,
    at: Option<Timestamp>,
    instance_id: Option<String>,
    trace_id: Option<String>,
    call_id: Option<String>,
}

/// The `record` kinds the filters tell apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineRecord {
    Resource,
    /// A log, span, metric or environment line.
    Other,
}

impl LineFacts {
    /// A line that is not a JSON object has no facts.
    fn of(line: &str) -> Self {
        let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(line) else {
            return Self::default();
        };
        let text = |value: Option<&serde_json::Value>| {
            value.and_then(serde_json::Value::as_str).map(str::to_owned)
        };
        let resource = fields.get("resource");
        Self {
            record: text(fields.get("record")).map(|record| {
                if record == "resource" {
                    LineRecord::Resource
                } else {
                    LineRecord::Other
                }
            }),
            at: text(fields.get("timestamp")).and_then(|at| at.parse().ok()),
            instance_id: text(fields.get(telemetry::fields::SERVICE_INSTANCE_ID)).or_else(|| {
                text(
                    resource
                        .and_then(|resource| resource.get(telemetry::fields::SERVICE_INSTANCE_ID)),
                )
            }),
            trace_id: text(fields.get(telemetry::fields::TRACE_ID)),
            call_id: text(
                fields
                    .get("attributes")
                    .and_then(|attributes| attributes.get(telemetry::fields::CALL_ID)),
            ),
        }
    }
}

// Calls `visit` with the facts of each line of the files in `from`. A missing file has none.
fn for_each_line_facts(from: &[PathBuf], mut visit: impl FnMut(LineFacts)) -> std::io::Result<()> {
    for source_path in from {
        let source = match File::open(source_path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for line in BufReader::new(source).lines() {
            visit(LineFacts::of(&line?));
        }
    }
    Ok(())
}

/// A call as its lines name it: its `call.id`, or the trace derived from it.
struct CallMatch {
    call_id: String,
    trace_id: String,
}

impl CallMatch {
    fn holds(&self, facts: &LineFacts) -> bool {
        facts.trace_id.as_deref() == Some(self.trace_id.as_str())
            || facts.call_id.as_deref() == Some(self.call_id.as_str())
    }
}

/// Picks one call's or one meeting's lines out of a recorder dump by their text, as the
/// telemetry crate writes them: compact JSON, so each needle is exact.
struct RecorderLines {
    needles: Vec<String>,
}

impl RecorderLines {
    fn of_call(call_id: &Path) -> Self {
        let call_id = call_id.display().to_string();
        Self {
            needles: vec![
                format!(
                    "\"trace_id\":\"{}\"",
                    telemetry::trace::TraceId::for_call(&call_id)
                ),
                format!("\"call.id\":\"{}\"", json_escaped(&call_id)),
            ],
        }
    }

    /// The meeting's lines: those with its `meeting.id`, as text or as a number, and those in
    /// the trace of any run on any of its input revisions so far.
    fn of_meeting(meeting_id: jobs::MeetingId, input_revision: i64) -> Self {
        let id = meeting_id.to_string();
        let run_traces = (0..=input_revision).map(|revision| {
            format!(
                "\"trace_id\":\"{}\"",
                telemetry::trace::TraceId::for_pipeline_run(&id, &revision.to_string())
            )
        });
        let id_needles = [
            format!("\"meeting.id\":\"{id}\""),
            format!("\"meeting.id\":{id},"),
            format!("\"meeting.id\":{id}}}"),
        ];
        Self {
            needles: id_needles.into_iter().chain(run_traces).collect(),
        }
    }

    /// A dump header, a resource line, or a line one of the needles finds.
    fn holds(&self, line: &str) -> bool {
        line.starts_with(r#"{"record":"dump""#)
            || line.starts_with(r#"{"record":"resource""#)
            || self
                .needles
                .iter()
                .any(|needle| line.contains(needle.as_str()))
    }
}

// From the call's start to its end on the status board. When the board no longer has the call,
// from the call log's first line to its last. Widened by the margin on both sides.
fn call_window(request: &ReportRequest, saved_at: Timestamp) -> TimeWindow {
    let board_time = |key: &str| {
        request
            .call_status
            .as_ref()?
            .get(key)?
            .as_str()?
            .parse::<Timestamp>()
            .ok()
    };
    let (first_line_at, last_line_at) = call_log_span(&request.call_id).unzip();
    let start = board_time("started_at")
        .or(first_line_at)
        .unwrap_or(saved_at - RECENT_WINDOW);
    let end = board_time("ended_at")
        .or_else(|| {
            // A call the board still has but has not ended runs to now.
            if request.call_status.is_some() {
                None
            } else {
                last_line_at
            }
        })
        .unwrap_or(saved_at);
    TimeWindow {
        start: start - CALL_WINDOW_MARGIN,
        end: end + CALL_WINDOW_MARGIN,
    }
}

// The `received_at` of the call log's first and last lines.
fn call_log_span(call_log: &Path) -> Option<(Timestamp, Timestamp)> {
    let file = File::open(call_log).ok()?;
    let received_at = |line: &str| -> Option<Timestamp> {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        value.get("received_at")?.as_str()?.parse().ok()
    };
    let mut first = None;
    let mut last = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Some(at) = received_at(&line) {
            first.get_or_insert(at);
            last = Some(at);
        }
    }
    first.zip(last)
}

/// `<service>-log.jsonl`, a service's lines in a report.
fn service_log_file(service: telemetry::Service) -> String {
    format!("{}-log.jsonl", service.name())
}

/// The daily files `<prefix>-<UTC date>.jsonl` outside the service logs, such as `env` and
/// `metrics`, oldest first.
fn daily_row_files(log_dir: &Path, prefix: &str) -> std::io::Result<Vec<PathBuf>> {
    let name_start = format!("{prefix}-");
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy())
                .is_some_and(|name| name.starts_with(&name_start) && name.ends_with(".jsonl"))
        })
        .collect();
    // The UTC date in the name sorts the files by day.
    files.sort();
    Ok(files)
}

// A string as it appears inside a JSON string, without the quotes.
fn json_escaped(text: &str) -> String {
    let quoted = serde_json::Value::String(text.to_owned()).to_string();
    quoted
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map_or_else(|| text.to_owned(), str::to_owned)
}

/// The call log's file name without its extension, which names the call's debug folder.
fn call_stem(call_id: &Path) -> String {
    call_id.file_stem().map_or_else(
        || "call".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    )
}

fn received_at() -> serde_json::Value {
    serde_json::json!(Timestamp::now())
}
