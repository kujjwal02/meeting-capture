//! The tray icon and its menu. The menu is rebuilt on every job change and whenever a call opens
//! or ends. Dropping a menu closes it if it is open, so the once-a-second call status update
//! changes the text of the existing call lines instead. The icon shows the worst call state: grey
//! idle, a red dot while captions or audio record, amber at a problem. Each open call has an
//! audio line and "Stop recording this call", or "Resume recording" once stopped.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::tz::TimeZone;
use jobs::{JobState, Meeting, MeetingId};
use pipeline::settings::SettingsStore;
use pipeline::{Pipeline, has_user_edits};
use tauri::image::Image;
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt as _;

use crate::audio_capture::AudioState;
use crate::call_status::{BoardView, CallState, CallView, Moment, TrayLook, display_time_zone};
use crate::launch::{LaunchTarget, launch};
use crate::{AppState, meetings, updates};

const TRAY_ID: &str = "main";
const TOOLTIP_HEADING: &str = "Meeting Capture";
/// Big enough for a 22 pt menu bar on a 2x display.
const TRAY_ICON_PX: u32 = 44;
/// Each pixel's coverage is sampled on a grid this many samples wide, for a smooth edge.
const ICON_SUBSAMPLES: u32 = 4;
const ICON_DOT_PERCENT: u32 = 32;

// The look last set, so a rebuild sets the icon only when it changes.
static SHOWN_LOOK: Mutex<Option<TrayLook>> = Mutex::new(None);
// The call lines of the menu last composed, so a status update can change their text in place.
static CALL_LINES: Mutex<Option<CallLines>> = Mutex::new(None);
/// Windows copies at most 128 UTF-16 units of a tooltip, the terminator included.
const MAX_WINDOWS_TOOLTIP_UNITS: usize = 127;
const RECENT_MEETINGS: u32 = 5;
const MAX_TITLE_CHARS: usize = 40;
const MAX_FAILURE_CHARS: usize = 90;
const MAX_REASON_CHARS: usize = 80;

// Menu ids are text, so each action round-trips through `id` and `parse`.
enum MenuAction {
    OpenNote(MeetingId),
    Retry(MeetingId),
    OverwriteEdits(MeetingId),
    StopRecording(PathBuf),
    ResumeRecording(PathBuf),
    RetryFailed,
    OpenAccessibilitySettings,
    InstallUpdate,
    CheckForUpdates,
    InstallMeetExtension,
    StartAtLogin,
    OpenSettings,
    ShowLogFile,
    SaveDebugReport,
    SaveDebugReportWithText,
    Quit,
}

impl MenuAction {
    fn id(&self) -> String {
        match self {
            MenuAction::OpenNote(id) => format!("open-note:{id}"),
            MenuAction::Retry(id) => format!("retry:{id}"),
            MenuAction::OverwriteEdits(id) => format!("overwrite-edits:{id}"),
            MenuAction::StopRecording(call_id) => format!("stop-recording:{}", call_id.display()),
            MenuAction::ResumeRecording(call_id) => {
                format!("resume-recording:{}", call_id.display())
            }
            MenuAction::RetryFailed => "retry-failed".to_owned(),
            MenuAction::OpenAccessibilitySettings => "open-accessibility-settings".to_owned(),
            MenuAction::InstallUpdate => "install-update".to_owned(),
            MenuAction::CheckForUpdates => "check-for-updates".to_owned(),
            MenuAction::InstallMeetExtension => "install-meet-extension".to_owned(),
            MenuAction::StartAtLogin => "start-at-login".to_owned(),
            MenuAction::OpenSettings => "open-settings".to_owned(),
            MenuAction::ShowLogFile => "show-log-file".to_owned(),
            MenuAction::SaveDebugReport => "save-debug-report".to_owned(),
            MenuAction::SaveDebugReportWithText => "save-debug-report-with-text".to_owned(),
            MenuAction::Quit => "quit".to_owned(),
        }
    }

    fn parse(id: &str) -> Option<Self> {
        let meeting_id = |text: &str| text.parse().ok().map(MeetingId::new);
        if let Some(id) = id.strip_prefix("open-note:") {
            return meeting_id(id).map(MenuAction::OpenNote);
        }
        if let Some(id) = id.strip_prefix("retry:") {
            return meeting_id(id).map(MenuAction::Retry);
        }
        if let Some(id) = id.strip_prefix("overwrite-edits:") {
            return meeting_id(id).map(MenuAction::OverwriteEdits);
        }
        if let Some(call_id) = id.strip_prefix("stop-recording:") {
            return Some(MenuAction::StopRecording(PathBuf::from(call_id)));
        }
        if let Some(call_id) = id.strip_prefix("resume-recording:") {
            return Some(MenuAction::ResumeRecording(PathBuf::from(call_id)));
        }
        [
            MenuAction::RetryFailed,
            MenuAction::OpenAccessibilitySettings,
            MenuAction::InstallUpdate,
            MenuAction::CheckForUpdates,
            MenuAction::InstallMeetExtension,
            MenuAction::StartAtLogin,
            MenuAction::OpenSettings,
            MenuAction::ShowLogFile,
            MenuAction::SaveDebugReport,
            MenuAction::SaveDebugReportWithText,
            MenuAction::Quit,
        ]
        .into_iter()
        .find(|action| action.id() == id)
    }
}

/// The open call lines of a composed menu, and what else of the menu follows the call board.
struct CallLines {
    shape: CallMenuShape,
    entries: Vec<CallEntries>,
}

/// The lines of one open call whose text changes in place.
struct CallEntries {
    status: MenuItem<Wry>,
    audio: Option<MenuItem<Wry>>,
}

/// The parts of the menu that change only when a call opens or ends.
#[derive(PartialEq, Eq)]
struct CallMenuShape {
    open_calls: usize,
    /// Whether each open call has an audio line, and its stop or resume entry's label.
    recording_rows: Vec<(bool, &'static str)>,
    recent_lines: Vec<String>,
    is_huddle_permission_missing: bool,
}

impl CallMenuShape {
    fn of(view: &BoardView, audio: &HashMap<PathBuf, AudioState>, time_zone: &TimeZone) -> Self {
        Self {
            open_calls: view.calls.len(),
            recording_rows: view
                .calls
                .iter()
                .map(|call| {
                    let row = RecordingRow::of(call, audio);
                    (row.audio_line.is_some(), row.action_label)
                })
                .collect(),
            recent_lines: view
                .recent
                .iter()
                .map(|call| call.summary_line(time_zone))
                .collect(),
            is_huddle_permission_missing: view.is_huddle_permission_missing,
        }
    }
}

/// What the menu shows under a call's status line about its recording.
struct RecordingRow {
    audio_line: Option<String>,
    action: MenuAction,
    action_label: &'static str,
}

impl RecordingRow {
    fn of(call: &CallView, audio: &HashMap<PathBuf, AudioState>) -> Self {
        let (action, action_label) = match call.state {
            CallState::Stopped => (
                MenuAction::ResumeRecording(call.id.clone()),
                "Resume recording",
            ),
            CallState::Waiting | CallState::Recording | CallState::Problem(_) => (
                MenuAction::StopRecording(call.id.clone()),
                "Stop recording this call",
            ),
        };
        Self {
            audio_line: audio.get(&call.id).map(|state| state.label()),
            action,
            action_label,
        }
    }
}

fn audio_states(app: &AppHandle) -> HashMap<PathBuf, AudioState> {
    app.state::<AppState>().audio_capture.states()
}

pub(crate) fn create(app: &AppHandle) -> tauri::Result<()> {
    let view = board_view(app);
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(tray_icon(TrayLook::Idle))
        .tooltip(TOOLTIP_HEADING)
        .menu(&compose_menu(app, &view)?)
        .on_menu_event(|app, event| handle_menu_event(app, &event))
        .build(app)?;
    *SHOWN_LOOK.lock().unwrap_or_else(PoisonError::into_inner) = Some(TrayLook::Idle);
    Ok(())
}

fn board_view(app: &AppHandle) -> BoardView {
    app.state::<AppState>().calls.status().view(Moment::now())
}

/// Updates the icon, the tooltip and the call lines from the call board. Rebuilds the menu only
/// when a call opened or ended since it was composed. Safe from any thread.
pub(crate) fn show_call_status(app: &AppHandle) {
    let main_app = app.clone();
    let queued = app.run_on_main_thread(move || {
        let Some(tray) = main_app.tray_by_id(TRAY_ID) else {
            return;
        };
        let view = board_view(&main_app);
        if let Err(error) = show_look(&main_app, &tray, &view) {
            tracing::error!(exception.message = %error, "could not set the tray icon");
        }
        let time_zone = display_time_zone(&main_app.state::<AppState>());
        let audio = audio_states(&main_app);
        let shape = CallMenuShape::of(&view, &audio, &time_zone);
        let is_updated = {
            let lines = CALL_LINES.lock().unwrap_or_else(PoisonError::into_inner);
            match lines.as_ref() {
                Some(lines) if lines.shape == shape => {
                    update_call_lines(lines, &view, &audio, &time_zone);
                    true
                }
                Some(_) | None => false,
            }
        };
        if !is_updated
            && let Err(error) =
                compose_menu(&main_app, &view).and_then(|menu| tray.set_menu(Some(menu)))
        {
            tracing::error!(exception.message = %error, "could not rebuild the tray menu");
        }
    });
    if let Err(error) = queued {
        tracing::error!(exception.message = %error, "could not queue a tray status update");
    }
}

// The status tick changes only text, so an open menu stays open.
fn update_call_lines(
    lines: &CallLines,
    view: &BoardView,
    audio: &HashMap<PathBuf, AudioState>,
    time_zone: &TimeZone,
) {
    for (entries, call) in lines.entries.iter().zip(&view.calls) {
        let audio_line = RecordingRow::of(call, audio).audio_line;
        let status_set = entries.status.set_text(call.status_line(time_zone));
        let updated = status_set.and_then(|()| match (&entries.audio, audio_line) {
            (Some(entry), Some(line)) => entry.set_text(line),
            (Some(_) | None, _) => Ok(()),
        });
        if let Err(error) = updated {
            tracing::error!(exception.message = %error, "could not update a tray menu line");
        }
    }
}

/// Rebuilds the menu from the job store. Safe from any thread.
pub(crate) fn refresh(app: &AppHandle) {
    // Composing on the main thread orders the rebuilds, so the menu never ends on an older
    // snapshot than the last state change.
    let main_app = app.clone();
    let queued = app.run_on_main_thread(move || {
        let Some(tray) = main_app.tray_by_id(TRAY_ID) else {
            // The listener can report before the tray exists; `create` composes the first menu.
            return;
        };
        let view = board_view(&main_app);
        if let Err(error) = show_look(&main_app, &tray, &view) {
            tracing::error!(exception.message = %error, "could not set the tray icon");
        }
        if let Err(error) =
            compose_menu(&main_app, &view).and_then(|menu| tray.set_menu(Some(menu)))
        {
            tracing::error!(exception.message = %error, "could not rebuild the tray menu");
        }
    });
    if let Err(error) = queued {
        tracing::error!(exception.message = %error, "could not queue a tray menu rebuild");
    }
}

// The icon follows the worst call state, and the tooltip carries each call's status line.
fn show_look(app: &AppHandle, tray: &TrayIcon, view: &BoardView) -> tauri::Result<()> {
    let audio = &app.state::<AppState>().audio_capture;
    let look = if audio.is_failing() {
        TrayLook::Problem
    } else {
        match view.tray_look() {
            TrayLook::Idle if audio.is_recording() => TrayLook::Recording,
            look @ (TrayLook::Idle | TrayLook::Recording | TrayLook::Problem) => look,
        }
    };
    let mut shown = SHOWN_LOOK.lock().unwrap_or_else(PoisonError::into_inner);
    if *shown != Some(look) {
        tray.set_icon(Some(tray_icon(look)))?;
        *shown = Some(look);
    }
    let time_zone = display_time_zone(&app.state::<AppState>());
    let mut tooltip = vec![TOOLTIP_HEADING.to_owned()];
    if view.is_huddle_permission_missing {
        tooltip.push(HUDDLE_PERMISSION_LINE.to_owned());
    }
    let audio_states = audio.states();
    for call in &view.calls {
        tooltip.push(call.status_line(&time_zone));
        if let Some(state) = audio_states.get(&call.id) {
            tooltip.push(state.label());
        }
    }
    let tooltip = tooltip.join("\n");
    // Windows copies a longer tooltip with no terminator, which shows garbage after it.
    let tooltip = if cfg!(windows) {
        truncate_utf16(&tooltip, MAX_WINDOWS_TOOLTIP_UNITS)
    } else {
        tooltip
    };
    tray.set_tooltip(Some(tooltip))
}

// The longest prefix of whole characters that fits in `max_units` UTF-16 units.
fn truncate_utf16(text: &str, max_units: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= max_units
        })
        .collect()
}

// A dot in the middle of a clear square.
fn tray_icon(look: TrayLook) -> Image<'static> {
    let [red, green, blue] = match look {
        TrayLook::Idle => [142, 142, 147],
        TrayLook::Recording => [255, 59, 48],
        TrayLook::Problem => [255, 159, 10],
    };
    // In half-sample units, so sample centres fall on whole numbers.
    let centre = i64::from(TRAY_ICON_PX * ICON_SUBSAMPLES);
    let radius = i64::from(TRAY_ICON_PX * ICON_SUBSAMPLES * 2 * ICON_DOT_PERCENT / 100);
    let samples_per_pixel = ICON_SUBSAMPLES * ICON_SUBSAMPLES;
    let mut rgba = Vec::new();
    for y in 0..TRAY_ICON_PX {
        for x in 0..TRAY_ICON_PX {
            let mut covered = 0;
            for sample_y in 0..ICON_SUBSAMPLES {
                for sample_x in 0..ICON_SUBSAMPLES {
                    let dx = i64::from(2 * (x * ICON_SUBSAMPLES + sample_x) + 1) - centre;
                    let dy = i64::from(2 * (y * ICON_SUBSAMPLES + sample_y) + 1) - centre;
                    if dx * dx + dy * dy <= radius * radius {
                        covered += 1;
                    }
                }
            }
            let alpha = u8::try_from(covered * 255 / samples_per_pixel).unwrap_or(u8::MAX);
            rgba.extend([red, green, blue, alpha]);
        }
    }
    Image::new_owned(rgba, TRAY_ICON_PX, TRAY_ICON_PX)
}

fn compose_menu(app: &AppHandle, view: &BoardView) -> tauri::Result<Menu<Wry>> {
    let state = app.state::<AppState>();
    let menu = Menu::new(app)?;
    if let Some(reason) = &state.pipeline_error {
        let label = format!("Not running: {}", shorten(reason, MAX_REASON_CHARS));
        menu.append(&label_entry(app, &label)?)?;
    }
    if view.is_huddle_permission_missing {
        menu.append(&action_entry(
            app,
            &MenuAction::OpenAccessibilitySettings,
            HUDDLE_PERMISSION_LINE,
            true,
        )?)?;
    }
    append_browser_call_status(app, &menu, &state)?;
    append_calls(app, &menu, &state, view)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    if let Some(pipeline) = &state.pipeline {
        append_google_accounts(app, &menu, &state.settings)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        append_meetings(app, &menu, pipeline)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }

    if let Some(version) = state.updates.ready_version() {
        let label = format!("Install update {version} and restart");
        menu.append(&action_entry(
            app,
            &MenuAction::InstallUpdate,
            &label,
            true,
        )?)?;
    } else {
        menu.append(&check_for_updates_entry(app, &state)?)?;
    }
    menu.append(&action_entry(
        app,
        &MenuAction::InstallMeetExtension,
        "Install mc-meet in Chrome…",
        true,
    )?)?;
    menu.append(&start_at_login_entry(app)?)?;
    menu.append(&action_entry(
        app,
        &MenuAction::OpenSettings,
        "Settings…",
        true,
    )?)?;
    menu.append(&action_entry(
        app,
        &MenuAction::ShowLogFile,
        "Show log file",
        true,
    )?)?;
    let has_call = view.current_or_last_call().is_some();
    menu.append(&action_entry(
        app,
        &MenuAction::SaveDebugReport,
        "Save debug report",
        has_call,
    )?)?;
    menu.append(&action_entry(
        app,
        &MenuAction::SaveDebugReportWithText,
        "Save debug report with caption text",
        has_call,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&action_entry(app, &MenuAction::Quit, "Quit", true)?)?;
    Ok(menu)
}

const HUDDLE_PERMISSION_LINE: &str = "Slack huddles: allow Accessibility to read captions";

// One line for the extension.
fn append_browser_call_status(
    app: &AppHandle,
    menu: &Menu<Wry>,
    state: &AppState,
) -> tauri::Result<()> {
    let status = state.browser_calls.status();
    let extension_label = match (status.is_connected, status.is_extension_outdated) {
        (false, _) => "mc-meet: not connected",
        (true, false) => "mc-meet: connected",
        (true, true) => "mc-meet: connected, reload it in chrome://extensions",
    };
    menu.append(&label_entry(app, extension_label)?)
}

// A status line per open call, then the calls that ended in this run.
fn append_calls(
    app: &AppHandle,
    menu: &Menu<Wry>,
    state: &AppState,
    view: &BoardView,
) -> tauri::Result<()> {
    let time_zone = display_time_zone(state);
    let audio = audio_states(app);
    let mut entries = Vec::new();
    for call in &view.calls {
        let status = label_entry(app, &call.status_line(&time_zone))?;
        menu.append(&status)?;
        let row = RecordingRow::of(call, &audio);
        let audio_entry = match &row.audio_line {
            Some(audio_line) => {
                let entry = label_entry(app, audio_line)?;
                menu.append(&entry)?;
                Some(entry)
            }
            None => None,
        };
        entries.push(CallEntries {
            status,
            audio: audio_entry,
        });
        menu.append(&action_entry(app, &row.action, row.action_label, true)?)?;
    }
    *CALL_LINES.lock().unwrap_or_else(PoisonError::into_inner) = Some(CallLines {
        shape: CallMenuShape::of(view, &audio, &time_zone),
        entries,
    });
    if view.recent.is_empty() {
        return Ok(());
    }
    let recent = Submenu::new(app, "Recent calls", true)?;
    for call in &view.recent {
        recent.append(&label_entry(app, &call.summary_line(&time_zone))?)?;
    }
    menu.append(&recent)
}

// A line per account. One that needs to sign in again opens the settings window.
fn append_google_accounts(
    app: &AppHandle,
    menu: &Menu<Wry>,
    settings: &SettingsStore,
) -> tauri::Result<()> {
    let accounts = settings.google().statuses();
    if accounts.is_empty() {
        return menu.append(&action_entry(
            app,
            &MenuAction::OpenSettings,
            "Calendar: no Google account; add one",
            true,
        )?);
    }
    for account in accounts {
        if account.needs_sign_in {
            let label = format!("Calendar: {} (sign in again)", account.email);
            menu.append(&action_entry(app, &MenuAction::OpenSettings, &label, true)?)?;
        } else {
            menu.append(&label_entry(app, &format!("Calendar: {}", account.email))?)?;
        }
    }
    Ok(())
}

fn append_meetings(app: &AppHandle, menu: &Menu<Wry>, pipeline: &Pipeline) -> tauri::Result<()> {
    let store = pipeline.store();
    let recent = match store.recent(RECENT_MEETINGS) {
        Ok(recent) => recent,
        Err(error) => {
            tracing::error!(exception.message = %error, "could not list recent meetings");
            menu.append(&label_entry(app, "Job store error; see the log")?)?;
            return Ok(());
        }
    };
    if recent.is_empty() {
        menu.append(&label_entry(app, "No meetings yet")?)?;
    }
    for meeting in &recent {
        menu.append(&meeting_submenu(app, pipeline, meeting)?)?;
    }
    let failed_count = match store.failed() {
        Ok(failed) => failed.len(),
        Err(error) => {
            tracing::error!(exception.message = %error, "could not list failed meetings");
            menu.append(&label_entry(app, "Job store error; see the log")?)?;
            0
        }
    };
    if failed_count > 0 {
        let label = format!("Retry failed ({failed_count})");
        menu.append(&action_entry(app, &MenuAction::RetryFailed, &label, true)?)?;
    }
    Ok(())
}

fn meeting_submenu(
    app: &AppHandle,
    pipeline: &Pipeline,
    meeting: &Meeting,
) -> tauri::Result<Submenu<Wry>> {
    let time_zone = &pipeline.settings().time_zone;
    let started_at = meeting
        .started_at
        .to_zoned(time_zone.clone())
        .strftime("%d %b %H:%M %Z");
    let title = shorten(
        meeting.title.as_deref().unwrap_or("Untitled call"),
        MAX_TITLE_CHARS,
    );
    let job_state = meeting.state();
    let submenu = Submenu::new(
        app,
        format!("{started_at}  {title}  ({})", job_state.label()),
        true,
    )?;
    let is_failed = match &job_state {
        JobState::Failed { cause } => {
            let message = shorten(&cause.message, MAX_FAILURE_CHARS);
            submenu.append(&label_entry(app, &message)?)?;
            true
        }
        JobState::ExtractionPending { retry: Some(retry) } => {
            let label = format!("Extraction pending, {}", retry.describe(time_zone));
            submenu.append(&label_entry(app, &label)?)?;
            let last_error = shorten(retry.last_error(), MAX_FAILURE_CHARS);
            submenu.append(&label_entry(app, &last_error)?)?;
            false
        }
        JobState::Captured
        | JobState::Matched
        | JobState::Extracted
        | JobState::Written { .. }
        | JobState::ExtractionPending { retry: None } => false,
    };
    submenu.append(&action_entry(
        app,
        &MenuAction::OpenNote(meeting.id),
        "Open note",
        meeting.note.path().is_some(),
    )?)?;
    submenu.append(&action_entry(
        app,
        &MenuAction::Retry(meeting.id),
        "Retry",
        is_failed,
    )?)?;
    // A rerun splices into an edited note; this is for a note whose markers it cannot place.
    if is_failed && has_user_edits(&meeting.note) {
        submenu.append(&action_entry(
            app,
            &MenuAction::OverwriteEdits(meeting.id),
            "Overwrite my edits",
            true,
        )?)?;
    }
    Ok(submenu)
}

fn check_for_updates_entry(app: &AppHandle, state: &AppState) -> tauri::Result<MenuItem<Wry>> {
    // A dev build has no bundle to replace, so it never checks.
    if cfg!(debug_assertions) {
        return label_entry(app, "Check for updates (release builds only)");
    }
    if state.updates.is_checking() {
        return label_entry(app, "Checking for updates…");
    }
    action_entry(app, &MenuAction::CheckForUpdates, "Check for updates", true)
}

// A login item from a dev build would start the debug binary at login on the real folders, and
// its single-instance lock would then stop the installed app.
#[cfg(debug_assertions)]
fn start_at_login_entry(app: &AppHandle) -> tauri::Result<MenuItem<Wry>> {
    label_entry(app, "Start at login (release builds only)")
}

#[cfg(not(debug_assertions))]
fn start_at_login_entry(app: &AppHandle) -> tauri::Result<tauri::menu::CheckMenuItem<Wry>> {
    let is_start_at_login = app.autolaunch().is_enabled().unwrap_or_else(|error| {
        tracing::warn!(exception.message = %error, "could not read the login item");
        false
    });
    tauri::menu::CheckMenuItem::with_id(
        app,
        MenuAction::StartAtLogin.id(),
        "Start at login",
        true,
        is_start_at_login,
        None::<&str>,
    )
}

fn handle_menu_event(app: &AppHandle, event: &MenuEvent) {
    let Some(action) = MenuAction::parse(event.id().as_ref()) else {
        tracing::warn!(menu.item.id = event.id().as_ref(), "unknown tray menu item");
        return;
    };
    let state = app.state::<AppState>();
    match action {
        MenuAction::OpenNote(id) => open_note(app, &state, id),
        MenuAction::Retry(id) => {
            if let Some(pipeline) = &state.pipeline {
                let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
                tauri::async_runtime::spawn_blocking(move || meetings::run(&app, &pipeline, id));
            }
        }
        MenuAction::OverwriteEdits(id) => {
            if let Some(pipeline) = &state.pipeline {
                let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
                tauri::async_runtime::spawn_blocking(move || {
                    meetings::overwrite_edits(&app, &pipeline, id);
                });
            }
        }
        MenuAction::StopRecording(call_id) => crate::audio_capture::stop_call(app, &call_id),
        MenuAction::ResumeRecording(call_id) => crate::audio_capture::resume_call(app, &call_id),
        MenuAction::RetryFailed => {
            if let Some(pipeline) = &state.pipeline {
                let (app, pipeline) = (app.clone(), Arc::clone(pipeline));
                tauri::async_runtime::spawn_blocking(move || {
                    meetings::retry_failed(&app, &pipeline);
                });
            }
        }
        MenuAction::OpenAccessibilitySettings => {
            const ACCESSIBILITY_PANE: &str =
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
            if let Err(error) = app.opener().open_url(ACCESSIBILITY_PANE, None::<&str>) {
                tracing::error!(exception.message = %error, "could not open the Accessibility settings");
            }
        }
        MenuAction::InstallUpdate => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || updates::install_and_restart(&app));
        }
        MenuAction::CheckForUpdates => updates::check_now(app),
        MenuAction::InstallMeetExtension => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || crate::meet_extension::show_setup(&app));
        }
        MenuAction::StartAtLogin => toggle_start_at_login(app),
        MenuAction::OpenSettings => crate::settings_window::open(app),
        MenuAction::SaveDebugReport => save_debug_report(app, false),
        MenuAction::SaveDebugReportWithText => save_debug_report(app, true),
        MenuAction::ShowLogFile => launch(
            app,
            &LaunchTarget::RevealFile(
                &state
                    .settings
                    .app_dirs()
                    .log_dir()
                    .join(crate::LOG_FILE_NAME),
            ),
        ),
        MenuAction::Quit => app.exit(0),
    }
}

fn save_debug_report(app: &AppHandle, is_with_text: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = crate::call_debug::save_debug_report(&app, None, is_with_text) {
            tracing::error!(exception.message = %error, "could not save a debug report");
            crate::notify(&app, "Debug report not saved", &error.to_string());
        }
    });
}

fn open_note(app: &AppHandle, state: &AppState, id: MeetingId) {
    let Some(pipeline) = &state.pipeline else {
        return;
    };
    match pipeline.store().get(id) {
        Ok(meeting) => {
            if let Some(note_path) = meeting.note.path() {
                launch(app, &LaunchTarget::Note(note_path));
            } else {
                tracing::warn!(meeting.id = %id, "meeting has no note yet");
            }
        }
        Err(error) => tracing::error!(exception.message = %error, "could not read the meeting"),
    }
}

fn toggle_start_at_login(app: &AppHandle) {
    let autolaunch = app.autolaunch();
    let toggled = match autolaunch.is_enabled() {
        Ok(true) => autolaunch.disable(),
        Ok(false) => autolaunch.enable(),
        Err(error) => Err(error),
    };
    if let Err(error) = toggled {
        tracing::error!(exception.message = %error, "could not change the login item");
    }
    refresh(app);
}

fn action_entry(
    app: &AppHandle,
    action: &MenuAction,
    label: &str,
    is_enabled: bool,
) -> tauri::Result<MenuItem<Wry>> {
    MenuItem::with_id(app, action.id(), label, is_enabled, None::<&str>)
}

fn label_entry(app: &AppHandle, label: &str) -> tauri::Result<MenuItem<Wry>> {
    MenuItem::new(app, label, false, None::<&str>)
}

pub(crate) fn shorten(text: &str, max_chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}
