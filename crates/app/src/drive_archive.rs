//! The Drive archive sweep. With `[drive] enabled` and a signed-in Drive account, it moves the
//! media of each eligible call folder to Google Drive through `Pipeline::archive_folder`, then
//! points the notes of the folders it emptied at the Drive copies.
//!
//! A sweep runs at each hourly media check, once a meeting's note is written, after the
//! settings window turns the archive on, and after a Drive sign-in. One runs at a time, off the
//! async runtime. A full Drive or an account that must sign in again pauses the archive until
//! the next hourly check or a new sign-in. A network fault ends the sweep; the next one goes on
//! from each folder's `drive.json`. The sweep reads `[drive]` again before each folder and each
//! deletion, and stops once the archive is off or its account changed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use call_folder::drive_record::DriveStanding;
use pipeline::Pipeline;
use pipeline::archive::{
    ArchiveError, ArchiveHold, ArchiveOutcome, ArchiveReadiness, ArchiveRun, CallMeetings,
};
use pipeline::drive::{DriveClient, DriveError};
use pipeline::google::OAuthClient;
use tauri::{AppHandle, Manager as _};

use crate::AppState;

/// What started a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepTrigger {
    HourlyCheck,
    NoteWritten,
    SettingsSaved,
    SignedIn,
}

impl SweepTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::HourlyCheck => "hourly_check",
            Self::NoteWritten => "note_written",
            Self::SettingsSaved => "settings_saved",
            Self::SignedIn => "signed_in",
        }
    }

    /// Whether the trigger lifts a pause: the hourly check tries again, and a sign-in is the
    /// user's fix.
    fn lifts_pause(self) -> bool {
        match self {
            Self::HourlyCheck | Self::SignedIn => true,
            Self::NoteWritten | Self::SettingsSaved => false,
        }
    }
}

/// Where the archive stands, for the tray and the settings window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArchiveStatus {
    /// No sweep has finished since launch.
    NotRun,
    /// `[drive] enabled` with no Drive account signed in.
    NotSignedIn,
    Archived {
        archived_count: usize,
    },
    Uploading {
        archived_count: usize,
    },
    DriveFull {
        account: String,
    },
    SignInAgain {
        account: String,
    },
    /// Folders whose uploads are on another account's Drive wait for it to finish their deletion.
    OtherAccountWaits {
        waiting_count: usize,
        account: String,
    },
    Failed {
        message: String,
    },
}

impl ArchiveStatus {
    /// One line for the tray menu and the settings window.
    pub(crate) fn line(&self) -> String {
        match self {
            Self::NotRun => "Drive: archive not run yet".to_owned(),
            Self::NotSignedIn => "Drive: sign in to archive call media".to_owned(),
            Self::Archived { archived_count } => {
                format!("Drive: {}", archived_calls(*archived_count))
            }
            Self::Uploading { archived_count } => {
                format!("Drive: {}, uploading", archived_calls(*archived_count))
            }
            Self::DriveFull { account } => format!("Drive full: {account}"),
            Self::SignInAgain { account } => format!("Drive: sign in again ({account})"),
            Self::OtherAccountWaits {
                waiting_count,
                account,
            } => {
                if *waiting_count == 1 {
                    format!("Drive: 1 call waits for {account}")
                } else {
                    format!("Drive: {waiting_count} calls wait for {account}")
                }
            }
            Self::Failed { message } => format!("Drive: {message}"),
        }
    }

    /// Whether the tray turns amber: the archive is stopped until the user acts or the next try.
    pub(crate) fn is_problem(&self) -> bool {
        match self {
            Self::DriveFull { .. } | Self::SignInAgain { .. } | Self::Failed { .. } => true,
            // Nothing is lost while they wait, and the other folders go on.
            Self::NotRun
            | Self::NotSignedIn
            | Self::Archived { .. }
            | Self::Uploading { .. }
            | Self::OtherAccountWaits { .. } => false,
        }
    }

    /// Whether the settings window is where the user fixes it.
    pub(crate) fn needs_settings(&self) -> bool {
        match self {
            Self::NotSignedIn | Self::SignInAgain { .. } | Self::OtherAccountWaits { .. } => true,
            Self::NotRun
            | Self::Archived { .. }
            | Self::Uploading { .. }
            | Self::DriveFull { .. }
            | Self::Failed { .. } => false,
        }
    }
}

fn archived_calls(count: usize) -> String {
    if count == 1 {
        "1 call archived".to_owned()
    } else {
        format!("{count} calls archived")
    }
}

pub(crate) struct DriveArchive {
    status: Mutex<ArchiveStatus>,
    is_sweeping: AtomicBool,
    is_paused: AtomicBool,
}

impl DriveArchive {
    pub(crate) fn new() -> Self {
        Self {
            status: Mutex::new(ArchiveStatus::NotRun),
            is_sweeping: AtomicBool::new(false),
            is_paused: AtomicBool::new(false),
        }
    }

    pub(crate) fn status(&self) -> ArchiveStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Shows `status`. A full Drive or a sign-in pauses the archive.
    fn set_status(&self, app: &AppHandle, status: ArchiveStatus) {
        match status {
            ArchiveStatus::DriveFull { .. } | ArchiveStatus::SignInAgain { .. } => {
                self.is_paused.store(true, Ordering::Release);
            }
            ArchiveStatus::NotRun
            | ArchiveStatus::NotSignedIn
            | ArchiveStatus::Archived { .. }
            | ArchiveStatus::Uploading { .. }
            | ArchiveStatus::OtherAccountWaits { .. }
            | ArchiveStatus::Failed { .. } => {}
        }
        let mut shown = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if *shown == status {
            return;
        }
        *shown = status;
        drop(shown);
        crate::tray::refresh(app);
    }
}

/// Clears the sweep flag however the sweep ends, a panic included.
struct SweepGuard<'archive>(&'archive AtomicBool);

impl Drop for SweepGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Starts a sweep in the background, unless one runs or the archive is paused.
pub(crate) fn start_sweep(app: &AppHandle, trigger: SweepTrigger) {
    let archive = &app.state::<AppState>().drive_archive;
    if trigger.lifts_pause() {
        archive.is_paused.store(false, Ordering::Release);
    } else if archive.is_paused.load(Ordering::Acquire) {
        tracing::debug!(
            mc.archive.trigger = trigger.as_str(),
            "archive.sweep_skipped_paused"
        );
        return;
    }
    if archive.is_sweeping.swap(true, Ordering::AcqRel) {
        tracing::debug!(
            mc.archive.trigger = trigger.as_str(),
            "archive.sweep_already_running"
        );
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let archive = &app.state::<AppState>().drive_archive;
        let _guard = SweepGuard(&archive.is_sweeping);
        sweep(&app, trigger);
    });
}

fn sweep(app: &AppHandle, trigger: SweepTrigger) {
    let state = app.state::<AppState>();
    let archive = &state.drive_archive;
    let Some(pipeline) = state.pipeline.clone() else {
        return;
    };
    let _span = tracing::info_span!(
        "archive.sweep",
        mc.archive.trigger = trigger.as_str(),
        mc.archive.archived_folder_count = tracing::field::Empty,
        mc.archive.failed_folder_count = tracing::field::Empty,
    )
    .entered();
    let drive_account = match drive_account(&state) {
        Ok(drive_account) => drive_account,
        Err(status) => {
            archive.set_status(app, status);
            return;
        }
    };
    let calls_dir = pipeline.calls_dir();
    let folders = match call_folder::call_folders(&calls_dir) {
        Ok(folders) => folders,
        Err(error) => {
            tracing::error!(
                file.directory = %calls_dir.display(),
                exception.message = %error,
                "archive.calls_unreadable"
            );
            archive.set_status(
                app,
                ArchiveStatus::Failed {
                    message: format!("could not list the calls folder: {error}"),
                },
            );
            return;
        }
    };
    let call_meetings = match pipeline.call_meetings() {
        Ok(call_meetings) => call_meetings,
        Err(error) => {
            tracing::error!(exception.message = %error, "archive.meetings_unreadable");
            archive.set_status(
                app,
                ArchiveStatus::Failed {
                    message: error.to_string(),
                },
            );
            return;
        }
    };
    // Checked before connecting, so a sweep with nothing to do makes no Google call.
    let ready_folders = ready_folders(app, &pipeline, &folders, &call_meetings);
    if ready_folders.is_empty() {
        archive.set_status(
            app,
            ArchiveStatus::Archived {
                archived_count: archived_count(&folders),
            },
        );
        return;
    }
    archive.set_status(
        app,
        ArchiveStatus::Uploading {
            archived_count: archived_count(&folders),
        },
    );
    let drive = match connect(&state, drive_account.account, drive_account.refresh_token) {
        Ok(drive) => drive,
        Err(status) => {
            archive.set_status(app, status);
            return;
        }
    };

    let tally = archive_ready_folders(
        app,
        &pipeline,
        &drive,
        &drive_account.root_folder,
        &call_meetings,
        &ready_folders,
    );
    let emptied_folders = tally.emptied_folders;
    let failed_count = tally.failed_count;
    let stop_status = tally.stop_status;
    tracing::Span::current().record("mc.archive.archived_folder_count", emptied_folders.len());
    tracing::Span::current().record("mc.archive.failed_folder_count", failed_count);
    if !emptied_folders.is_empty() {
        // The finalize refresh path: each note's Recording section then links the Drive copies.
        if let Err(error) = pipeline.refresh_recordings(&emptied_folders) {
            tracing::error!(exception.message = %error, "refresh.failed");
        }
    }
    let status = stop_status.unwrap_or(ArchiveStatus::Archived {
        archived_count: archived_count(&folders),
    });
    archive.set_status(app, status);
}

/// What one sweep's uploads came to.
struct SweepTally {
    /// Folders whose local media the sweep deleted, whose notes now link Drive.
    emptied_folders: Vec<PathBuf>,
    failed_count: usize,
    /// Why the sweep ended early or what failed last, or the folders another account holds.
    stop_status: Option<ArchiveStatus>,
}

/// Archives each ready folder in turn. A full Drive or a sign-in pauses the archive and ends the
/// sweep; a network fault, the archive turned off, or another account ends it.
fn archive_ready_folders(
    app: &AppHandle,
    pipeline: &Pipeline,
    drive: &DriveClient,
    root_folder: &str,
    call_meetings: &CallMeetings,
    ready_folders: &[PathBuf],
) -> SweepTally {
    let state = app.state::<AppState>();
    let calls_dir = pipeline.calls_dir();
    let is_archive_on = || is_still_archiving(&state, drive);
    let mut emptied_folders = Vec::new();
    let mut failed_count = 0_usize;
    let mut stop_status = None;
    let mut other_account_waits: Option<(usize, String)> = None;
    for folder in ready_folders {
        if !is_archive_on() {
            tracing::info!("archive.sweep_stopped_archive_off");
            break;
        }
        // Read again per folder: a call can open during a sweep of hours.
        let open_call_folders = crate::call_capture::open_call_folders(app, &calls_dir);
        let run = ArchiveRun {
            drive,
            root_folder,
            open_call_folders: &open_call_folders,
            call_meetings,
            is_archive_on: &is_archive_on,
        };
        match pipeline.archive_folder(&run, folder) {
            Ok(ArchiveOutcome::Archived { deleted_count, .. }) => {
                if deleted_count > 0 {
                    emptied_folders.push(folder.clone());
                }
            }
            Ok(ArchiveOutcome::Held(ArchiveHold::OtherAccount { account })) => {
                let waits = other_account_waits.get_or_insert((0, account));
                waits.0 += 1;
            }
            Ok(ArchiveOutcome::Held(ArchiveHold::ArchiveTurnedOff)) => break,
            Ok(ArchiveOutcome::Held(_)) => {}
            Err(error) => {
                failed_count += 1;
                tracing::error!(
                    file.directory = %folder.display(),
                    exception.message = %error,
                    "archive.folder_failed"
                );
                if let Some(status) = paused_status(&error) {
                    stop_status = Some(status);
                    break;
                }
                let is_network = matches!(error, ArchiveError::Drive(DriveError::Network { .. }));
                stop_status = Some(ArchiveStatus::Failed {
                    message: error.to_string(),
                });
                // Every folder after it would wait for the same link.
                if is_network {
                    break;
                }
            }
        }
    }
    if stop_status.is_none()
        && let Some((waiting_count, account)) = other_account_waits
    {
        stop_status = Some(ArchiveStatus::OtherAccountWaits {
            waiting_count,
            account,
        });
    }
    SweepTally {
        emptied_folders,
        failed_count,
        stop_status,
    }
}

/// The folders a run would upload or finish deleting now. A folder that cannot be checked is
/// logged and left for the next sweep.
fn ready_folders(
    app: &AppHandle,
    pipeline: &Pipeline,
    folders: &[PathBuf],
    call_meetings: &CallMeetings,
) -> Vec<PathBuf> {
    let open_call_folders = crate::call_capture::open_call_folders(app, &pipeline.calls_dir());
    folders
        .iter()
        .filter(|folder| {
            match pipeline.archive_readiness(folder, &open_call_folders, call_meetings) {
                Ok(ArchiveReadiness::Ready(_) | ArchiveReadiness::DeletionOwed(_)) => true,
                Ok(ArchiveReadiness::Held(_)) => false,
                Err(error) => {
                    tracing::warn!(
                        file.directory = %folder.display(),
                        exception.message = %error,
                        "archive.readiness_unreadable"
                    );
                    false
                }
            }
        })
        .cloned()
        .collect()
}

/// Call folders whose every upload is recorded, with the local copies gone or not.
fn archived_count(folders: &[PathBuf]) -> usize {
    folders
        .iter()
        .filter(|folder| match DriveStanding::of(folder) {
            DriveStanding::Uploaded | DriveStanding::MediaDeleted => true,
            DriveStanding::Local | DriveStanding::Uploading | DriveStanding::Unreadable => false,
        })
        .count()
}

/// The `[drive]` account a sweep uses.
struct DriveAccount {
    account: String,
    refresh_token: String,
    root_folder: String,
}

/// The enabled, signed-in Drive account, or the status that says why there is none. Read from
/// the file, since `mc drive sign-in` writes it from another process.
fn drive_account(state: &AppState) -> Result<DriveAccount, ArchiveStatus> {
    let drive_settings = state.settings.reload_drive().map_err(|error| {
        tracing::error!(exception.message = %error, "archive.settings_unreadable");
        ArchiveStatus::Failed {
            message: error.to_string(),
        }
    })?;
    if !drive_settings.enabled {
        return Err(ArchiveStatus::NotRun);
    }
    let (Some(account), Some(refresh_token)) =
        (drive_settings.account, drive_settings.refresh_token)
    else {
        return Err(ArchiveStatus::NotSignedIn);
    };
    Ok(DriveAccount {
        account,
        refresh_token,
        root_folder: drive_settings.root_folder,
    })
}

/// Whether `[drive]` still has the archive on for the account the sweep connected as. Read from
/// the file each time, so a change in the settings window or by `mc drive` stops the sweep.
fn is_still_archiving(state: &AppState, drive: &DriveClient) -> bool {
    drive_account(state).is_ok_and(|drive_account| drive_account.account == drive.account())
}

/// The Drive client, or the status that says why there is none.
fn connect(
    state: &AppState,
    account: String,
    refresh_token: String,
) -> Result<DriveClient, ArchiveStatus> {
    let failed = |message: String| ArchiveStatus::Failed { message };
    let file = state
        .settings
        .read()
        .map_err(|error| failed(error.to_string()))?;
    let client = OAuthClient::from_settings(&file.google)
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| failed("no Google OAuth client is set".to_owned()))?;
    DriveClient::connect(client, account, refresh_token).map_err(|error| {
        tracing::error!(exception.message = %error, "archive.connect_failed");
        drive_paused_status(&error).unwrap_or_else(|| failed(error.to_string()))
    })
}

/// The status of an error that pauses the archive: a full Drive, or an account that must sign
/// in again.
fn paused_status(error: &ArchiveError) -> Option<ArchiveStatus> {
    if !error.stops_archiving() {
        return None;
    }
    if let ArchiveError::Drive(drive_error) = error {
        return drive_paused_status(drive_error);
    }
    None
}

fn drive_paused_status(error: &DriveError) -> Option<ArchiveStatus> {
    match error {
        DriveError::QuotaExceeded { account } => Some(ArchiveStatus::DriveFull {
            account: account.clone(),
        }),
        DriveError::SignInAgain { account } => Some(ArchiveStatus::SignInAgain {
            account: account.clone(),
        }),
        DriveError::Network { .. }
        | DriveError::NotFound { .. }
        | DriveError::Trashed { .. }
        | DriveError::Other { .. }
        | DriveError::LocalFile { .. }
        | DriveError::VerificationFailed { .. } => None,
    }
}
