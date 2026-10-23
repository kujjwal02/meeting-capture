//! Debug material for calls that go wrong: frames mc-meet could not decode, the mc-meet events
//! before the first problem, page snapshots, and the debug report the user saves. It all stays
//! in the app data folder: raw frames and events hold call speech.

mod redaction;

use std::collections::{HashMap, VecDeque};
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
const FRAMES_FILE: &str = "frames.jsonl";
const LEAD_UP_FILE: &str = "lead-up.jsonl";
const PAGE_SNAPSHOTS_FILE: &str = "page-snapshots.jsonl";
const CALL_STATUS_FILE: &str = "call-status.json";
const APP_LOG_FILE: &str = "app-log.jsonl";

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
    app_log: PathBuf,
    tallies: Mutex<HashMap<PathBuf, DebugTally>>,
}

#[derive(Default)]
struct DebugTally {
    lead_up: VecDeque<(Instant, serde_json::Value)>,
    is_lead_up_written: bool,
    failed_frames: u32,
    page_snapshots: u32,
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
}

impl CallDebug {
    pub(crate) fn new(app_dirs: &AppDirs) -> Self {
        Self {
            call_folders_dir: app_dirs.call_debug_dir(),
            report_dir: app_dirs.debug_report_dir(),
            app_log: app_dirs.log_dir().join(crate::LOG_FILE_NAME),
            tallies: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, DebugTally>> {
        self.tallies.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The debug folder of the call whose log is `call_id`.
    fn call_folder(&self, call_id: &Path) -> PathBuf {
        self.call_folders_dir.join(call_stem(call_id))
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

    fn append_line(&self, call_id: &Path, file_name: &str, line: &serde_json::Value) {
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

    /// Deletes debug folders and reports older than 14 days.
    pub(crate) fn remove_expired(&self) {
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

    /// Gathers the call's app log lines, its call log and its debug files into one new folder,
    /// and returns the folder.
    pub(crate) fn save_report(&self, request: &ReportRequest) -> std::io::Result<PathBuf> {
        let stem = call_stem(&request.call_id);
        let suffix = if request.is_with_text {
            "-with-text"
        } else {
            ""
        };
        let saved_at = Timestamp::now();
        let folder = self.report_dir.join(format!(
            "{}-{stem}{suffix}",
            saved_at.strftime("%Y%m%dT%H%M%SZ")
        ));
        std::fs::create_dir_all(&folder)?;
        let mut redaction = if request.is_with_text {
            Redaction::KeepText
        } else {
            Redaction::TextLengths(Redactor::default())
        };

        let status = serde_json::json!({
            "saved_at": saved_at,
            "is_with_text": request.is_with_text,
            "app_version": env!("CARGO_PKG_VERSION"),
            "call": request.call_status,
        });
        std::fs::write(
            folder.join(CALL_STATUS_FILE),
            serde_json::to_vec_pretty(&status).map_err(std::io::Error::other)?,
        )?;

        // As the log writes them: JSON escapes the backslashes of a Windows path.
        let needles: Vec<String> = std::iter::once(request.call_id.to_string_lossy().into_owned())
            .chain(request.session_ids.iter().cloned())
            .map(|needle| json_escaped(&needle))
            .collect();
        copy_json_lines(
            &self.app_log,
            &folder.join(APP_LOG_FILE),
            |line| needles.iter().any(|needle| line.contains(needle.as_str())),
            &mut redaction,
        )?;

        if let Some(file_name) = request.call_id.file_name() {
            copy_json_lines(
                &request.call_id,
                &folder.join(file_name),
                |_| true,
                &mut redaction,
            )?;
        }

        let call_folder = self.call_folder(&request.call_id);
        if call_folder.is_dir() {
            let debug_folder = folder.join("debug");
            std::fs::create_dir_all(&debug_folder)?;
            for entry in std::fs::read_dir(&call_folder)?.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_file() {
                    copy_json_lines(
                        &path,
                        &debug_folder.join(entry.file_name()),
                        |_| true,
                        &mut redaction,
                    )?;
                }
            }
        }
        tracing::info!(
            call.id = %request.call_id.display(),
            file.directory = %folder.display(),
            "saved a debug report"
        );
        Ok(folder)
    }
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
    let request = ReportRequest {
        call_status: board.describe(&call_id, now),
        session_ids,
        call_id,
        is_with_text,
    };
    let folder = state.call_debug.save_report(&request)?;
    launch(app, &LaunchTarget::RevealFile(&folder));
    Ok(folder)
}

enum Redaction {
    KeepText,
    /// Only times, ids, versions and codes stay. Other text becomes its length, and frame bytes
    /// are left out.
    TextLengths(Redactor),
}

// Copies the lines of `from` that `is_wanted` keeps. A missing file copies nothing: an ended
// huddle may have no debug folder, and a call log may have been set aside.
fn copy_json_lines(
    from: &Path,
    to: &Path,
    is_wanted: impl Fn(&str) -> bool,
    redaction: &mut Redaction,
) -> std::io::Result<()> {
    let source = match File::open(from) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut target = BufWriter::new(File::create(to)?);
    for line in BufReader::new(source).lines() {
        let line = line?;
        if !is_wanted(&line) {
            continue;
        }
        match redaction {
            Redaction::KeepText => writeln!(target, "{line}")?,
            Redaction::TextLengths(redactor) => {
                let redacted = match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(mut value) => {
                        redactor.redact(&mut value);
                        value
                    }
                    Err(_) => serde_json::json!({ "unreadable_line_chars": line.chars().count() }),
                };
                serde_json::to_writer(&mut target, &redacted).map_err(std::io::Error::other)?;
                writeln!(target)?;
            }
        }
    }
    target.flush()
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
