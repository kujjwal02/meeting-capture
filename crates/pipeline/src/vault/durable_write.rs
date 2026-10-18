//! Moves a note into the vault in one rename, so Syncthing never sees a half-written note, and
//! syncs the folder so the rename survives power loss.

use std::io::Write as _;
use std::path::Path;

use crate::PipelineError;

/// Writes `contents` to a staging file outside the vault, then renames it over `note_path`.
/// `check_before_rename` runs once the staged file is on disk, as close to the rename as possible.
pub(super) fn write(
    staging_dir: &Path,
    note_path: &Path,
    contents: &str,
    check_before_rename: impl Fn() -> Result<(), PipelineError>,
) -> Result<(), PipelineError> {
    let write_error = |source| PipelineError::WriteNote {
        path: note_path.to_path_buf(),
        source,
    };
    let note_dir = note_path
        .parent()
        .ok_or_else(|| write_error(std::io::Error::other("note path has no folder")))?;
    create_dir_durably(note_dir).map_err(write_error)?;

    let staged = tempfile::NamedTempFile::new_in(staging_dir).map_err(write_error)?;
    let staged = fill(staged, contents).map_err(write_error)?;
    check_before_rename()?;
    match staged.persist(note_path) {
        Ok(_) => {}
        // A rename works only within one volume. For a vault on another disk, stage next to the
        // note under a dot-name, which Obsidian hides.
        Err(error) if error.error.kind() == std::io::ErrorKind::CrossesDevices => {
            let staged = tempfile::Builder::new()
                .prefix(".")
                .suffix(".tmp")
                .tempfile_in(note_dir)
                .map_err(write_error)?;
            let staged = fill(staged, contents).map_err(write_error)?;
            check_before_rename()?;
            staged
                .persist(note_path)
                .map_err(|error| write_error(error.error))?;
        }
        Err(error) => return Err(write_error(error.error)),
    }
    sync_dir(note_dir).map_err(write_error)
}

// A new month folder survives power loss only once its parent's entry is on disk too.
fn create_dir_durably(dir: &Path) -> std::io::Result<()> {
    let missing: Vec<&Path> = dir.ancestors().take_while(|path| !path.exists()).collect();
    std::fs::create_dir_all(dir)?;
    for created in missing.into_iter().rev() {
        if let Some(parent) = created.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

// A rename or a new entry is durable only once the folder is on disk. Windows has no folder fsync.
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "callers share one signature with the Unix fsync"
    )
)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn fill(
    mut staged: tempfile::NamedTempFile,
    contents: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    staged.write_all(contents.as_bytes())?;
    // Temp files are private (0600); a note gets the vault's usual mode.
    #[cfg(unix)]
    staged
        .as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
    staged.as_file().sync_all()?;
    Ok(staged)
}
