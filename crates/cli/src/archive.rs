//! `mc archive` and `mc restore`: a call folder's media to Google Drive and back.

use std::path::{Path, PathBuf};

use pipeline::Pipeline;
use pipeline::archive::{ArchiveOutcome, ArchiveReadiness, ArchiveRun};
use pipeline::drive::DriveClient;
use pipeline::settings::SettingsStore;

/// Which call folders a run takes.
enum ArchiveTarget {
    One(String),
    All,
}

impl ArchiveTarget {
    /// From `mc archive [<call>] [--all]`; clap already refuses both or neither.
    fn of(call: Option<String>, all: bool) -> anyhow::Result<Self> {
        match (call, all) {
            (Some(folder_or_meeting), false) => Ok(Self::One(folder_or_meeting)),
            (None, true) => Ok(Self::All),
            (Some(_), true) | (None, false) => {
                anyhow::bail!("give either a call folder or meeting ID, or --all")
            }
        }
    }
}

/// Whether a run uploads and deletes, or only prints what it would.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchiveMode {
    Archive,
    DryRun,
}

impl From<bool> for ArchiveMode {
    /// From `--dry-run`.
    fn from(dry_run: bool) -> Self {
        if dry_run { Self::DryRun } else { Self::Archive }
    }
}

#[derive(clap::Args)]
pub(crate) struct ArchiveArgs {
    /// A call folder name or a meeting ID.
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    call: Option<String>,
    /// Every call folder.
    #[arg(long)]
    all: bool,
    /// Print each call folder's eligibility and the space it would free. Uploads and deletes
    /// nothing, and needs no Drive sign-in.
    #[arg(long)]
    dry_run: bool,
}

pub(crate) fn run(
    pipeline: &Pipeline,
    settings_store: &SettingsStore,
    args: ArchiveArgs,
) -> anyhow::Result<()> {
    archive(
        pipeline,
        settings_store,
        &ArchiveTarget::of(args.call, args.all)?,
        args.dry_run.into(),
    )
}

fn archive(
    pipeline: &Pipeline,
    settings_store: &SettingsStore,
    target: &ArchiveTarget,
    mode: ArchiveMode,
) -> anyhow::Result<()> {
    let folders = match target {
        ArchiveTarget::One(folder_or_meeting) => {
            vec![pipeline.resolve_call_folder(folder_or_meeting)?]
        }
        ArchiveTarget::All => call_folder::call_folders(&pipeline.calls_dir())?,
    };
    match mode {
        ArchiveMode::DryRun => {
            // The app's list of the calls it holds open, which the one-hour rule alone would miss.
            let open_call_folders = pipeline.open_call_folders()?;
            print_readiness(pipeline, &folders, &open_call_folders)
        }
        ArchiveMode::Archive => archive_folders(pipeline, settings_store, &folders),
    }
}

fn print_readiness(
    pipeline: &Pipeline,
    folders: &[PathBuf],
    open_call_folders: &[PathBuf],
) -> anyhow::Result<()> {
    let call_meetings = pipeline.call_meetings()?;
    let mut ready_count = 0_usize;
    let mut freed_bytes = 0_u64;
    for folder in folders {
        let name = folder_name(folder);
        match pipeline.archive_readiness(folder, open_call_folders, &call_meetings) {
            Ok(ArchiveReadiness::Ready(plan)) => {
                ready_count += 1;
                freed_bytes += plan.freed_bytes;
                println!(
                    "{name}  ready  {}  uploads {}  frees {}  to {}",
                    plan.kind.as_str(),
                    plan.upload_names.join(", "),
                    describe_bytes(plan.freed_bytes),
                    plan.drive_folder_names.join("/"),
                );
            }
            Ok(ArchiveReadiness::DeletionOwed(owed)) => {
                ready_count += 1;
                freed_bytes += owed.freed_bytes;
                println!(
                    "{name}  ready  uploads done  deletes {}  frees {}",
                    owed.deleted_names.join(", "),
                    describe_bytes(owed.freed_bytes),
                );
            }
            Ok(ArchiveReadiness::Held(hold)) => {
                println!("{name}  held   {}", hold.describe());
            }
            Err(error) => println!("{name}  error  {error}"),
        }
    }
    if folders.len() > 1 {
        println!(
            "{ready_count} of {} call folders ready; {} would be freed",
            folders.len(),
            describe_bytes(freed_bytes)
        );
    }
    Ok(())
}

fn archive_folders(
    pipeline: &Pipeline,
    settings_store: &SettingsStore,
    folders: &[PathBuf],
) -> anyhow::Result<()> {
    let settings = settings_store.read()?;
    let drive = crate::drive::connect(&settings)?;
    let call_meetings = pipeline.call_meetings()?;
    // Asked for by hand, so the run goes on whatever the app's archive switch says.
    let is_archive_on = || true;
    let mut failed_count = 0_usize;
    let mut emptied_folders = Vec::new();
    let mut stop_error = None;
    for folder in folders {
        let name = folder_name(folder);
        // Read again per folder: a call can open during a run of hours. It is the app's list of
        // the calls it holds open, which the one-hour rule alone would miss.
        let open_call_folders = pipeline.open_call_folders()?;
        let run = ArchiveRun {
            drive: &drive,
            root_folder: settings.drive.root_folder.as_str(),
            open_call_folders: &open_call_folders,
            call_meetings: &call_meetings,
            is_archive_on: &is_archive_on,
        };
        match pipeline.archive_folder(&run, folder) {
            Ok(ArchiveOutcome::Archived {
                uploaded_count,
                deleted_count,
                freed_bytes,
            }) => {
                if deleted_count > 0 {
                    emptied_folders.push(folder.clone());
                }
                println!(
                    "{name}  archived  {uploaded_count} uploaded, {deleted_count} deleted, {} freed",
                    describe_bytes(freed_bytes)
                );
            }
            Ok(ArchiveOutcome::Held(hold)) => println!("{name}  held  {}", hold.describe()),
            Err(error) if error.stops_archiving() => {
                println!("{name}  stopped  {error}");
                stop_error = Some(error);
                break;
            }
            Err(error) => {
                failed_count += 1;
                let retry_note = if error.is_retryable() {
                    "; the next run goes on from here"
                } else {
                    ""
                };
                println!("{name}  failed  {error}{retry_note}");
            }
        }
    }
    if !emptied_folders.is_empty() {
        // As the app's sweep does: each note's Recording section then links the Drive copies.
        match pipeline.refresh_recordings(&emptied_folders) {
            Ok(refresh) if refresh.failed_count > 0 => println!(
                "{} notes could not link their Drive copies; the log says why",
                refresh.failed_count
            ),
            Ok(_) => {}
            Err(error) => println!("notes not updated to link their Drive copies: {error}"),
        }
    }
    if let Some(error) = stop_error {
        anyhow::bail!("archiving stopped: {error}");
    }
    if failed_count > 0 {
        anyhow::bail!(
            "{failed_count} call folders failed. Each deleted only files its drive.json records on Drive; the next run goes on from that record"
        );
    }
    Ok(())
}

pub(crate) fn restore(
    pipeline: &Pipeline,
    settings_store: &SettingsStore,
    folder_or_meeting: &str,
) -> anyhow::Result<()> {
    let folder = pipeline.resolve_call_folder(folder_or_meeting)?;
    let drive: DriveClient = crate::drive::connect(&settings_store.read()?)?;
    let restored = pipeline.restore_folder(&drive, &folder)?;
    let name = folder_name(&folder);
    for restored_name in &restored.restored_names {
        println!("{name}  restored  {restored_name}");
    }
    for kept_name in &restored.kept_names {
        println!("{name}  already here  {kept_name}");
    }
    for mixed_name in &restored.mixed_only_names {
        println!("{name}  only in call.mp4's mix  {mixed_name}");
    }
    println!(
        "{name}  {} restored; `mc archive` deletes them again once {} days have passed",
        restored.restored_names.len(),
        pipeline::archive::RESTORED_MEDIA_KEEP.as_hours() / 24
    );
    Ok(())
}

fn folder_name(folder: &Path) -> String {
    folder.file_name().map_or_else(
        || folder.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[expect(
    clippy::cast_precision_loss,
    reason = "a size shown to one decimal of a megabyte"
)]
fn describe_bytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}
