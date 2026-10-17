//! The tray icon and its menu. The menu is rebuilt on every job state change, since a macOS tray
//! menu does not refresh while it is open.

use std::sync::Arc;

use huddle_captions::HuddleHealth;
use jobs::{JobState, Meeting, MeetingId};
use pipeline::settings::SettingsStore;
use pipeline::{Pipeline, has_user_edits};
use tauri::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt as _;

use crate::huddles::HuddleStatus;
use crate::launch::{LaunchTarget, launch};
use crate::{AppState, Listener, meetings};

const TRAY_ID: &str = "main";
const RECENT_MEETINGS: u32 = 5;
const MAX_TITLE_CHARS: usize = 40;
const MAX_FAILURE_CHARS: usize = 90;
const MAX_REASON_CHARS: usize = 80;

// Menu ids are text, so each action round-trips through `id` and `parse`.
enum MenuAction {
    OpenNote(MeetingId),
    Retry(MeetingId),
    OverwriteEdits(MeetingId),
    RetryFailed,
    OpenAccessibilitySettings,
    StartAtLogin,
    OpenSettings,
    ShowLogFile,
    Quit,
}

impl MenuAction {
    fn id(&self) -> String {
        match self {
            MenuAction::OpenNote(id) => format!("open-note:{id}"),
            MenuAction::Retry(id) => format!("retry:{id}"),
            MenuAction::OverwriteEdits(id) => format!("overwrite-edits:{id}"),
            MenuAction::RetryFailed => "retry-failed".to_owned(),
            MenuAction::OpenAccessibilitySettings => "open-accessibility-settings".to_owned(),
            MenuAction::StartAtLogin => "start-at-login".to_owned(),
            MenuAction::OpenSettings => "open-settings".to_owned(),
            MenuAction::ShowLogFile => "show-log-file".to_owned(),
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
        [
            MenuAction::RetryFailed,
            MenuAction::OpenAccessibilitySettings,
            MenuAction::StartAtLogin,
            MenuAction::OpenSettings,
            MenuAction::ShowLogFile,
            MenuAction::Quit,
        ]
        .into_iter()
        .find(|action| action.id() == id)
    }
}

pub(crate) fn create(app: &AppHandle) -> tauri::Result<()> {
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("app icon".to_owned()))?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip("Meeting Capture")
        .menu(&compose_menu(app)?)
        .on_menu_event(|app, event| handle_menu_event(app, &event))
        .build(app)?;
    Ok(())
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
        if let Err(error) = compose_menu(&main_app).and_then(|menu| tray.set_menu(Some(menu))) {
            tracing::error!(exception.message = %error, "could not rebuild the tray menu");
        }
    });
    if let Err(error) = queued {
        tracing::error!(exception.message = %error, "could not queue a tray menu rebuild");
    }
}

fn compose_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let state = app.state::<AppState>();
    let menu = Menu::new(app)?;
    let listener_label = match state.listener() {
        Listener::Starting => "Starting…".to_owned(),
        Listener::Listening { port } => format!("Listening on 127.0.0.1:{port}"),
        Listener::Stopped { reason } => {
            format!("Not listening: {}", shorten(&reason, MAX_REASON_CHARS))
        }
    };
    menu.append(&label_entry(app, &listener_label)?)?;
    append_huddle_status(app, &menu, &state)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    if let Some(pipeline) = &state.pipeline {
        append_google_accounts(app, &menu, &state.settings)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
        append_meetings(app, &menu, pipeline)?;
        menu.append(&PredefinedMenuItem::separator(app)?)?;
    }

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
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&action_entry(app, &MenuAction::Quit, "Quit", true)?)?;
    Ok(menu)
}

// One line while a huddle runs or the watcher cannot run; nothing otherwise.
fn append_huddle_status(app: &AppHandle, menu: &Menu<Wry>, state: &AppState) -> tauri::Result<()> {
    let label = match state.huddle_status() {
        HuddleStatus::Idle => return Ok(()),
        HuddleStatus::PermissionNeeded => {
            return menu.append(&action_entry(
                app,
                &MenuAction::OpenAccessibilitySettings,
                "Slack huddles: allow Accessibility to read captions",
                true,
            )?);
        }
        HuddleStatus::Reading { channel, health } => {
            let title = shorten(&channel, MAX_TITLE_CHARS);
            match health {
                None => format!("Slack huddle {title}: looking for captions"),
                Some(HuddleHealth::Healthy) => format!("Slack huddle {title}: reading captions"),
                Some(HuddleHealth::Untested) => {
                    format!("Slack huddle {title}: reading captions (untested Slack version)")
                }
                Some(HuddleHealth::Degraded) => {
                    format!("Slack huddle {title}: captions partly read")
                }
                Some(HuddleHealth::CaptionsOff) => {
                    format!("Slack huddle {title}: captions are off")
                }
                Some(HuddleHealth::Broken) => format!("Slack huddle {title}: can't read captions"),
            }
        }
    };
    menu.append(&label_entry(app, &label)?)
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
        MenuAction::StartAtLogin => toggle_start_at_login(app),
        MenuAction::OpenSettings => crate::settings_window::open(app),
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

fn shorten(text: &str, max_chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}
