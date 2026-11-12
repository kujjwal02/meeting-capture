//! `open-calls.json` in the data folder: the call folders the app holds open, so `mc archive`
//! leaves them alone as the app's own sweep does. The app writes it at launch and at each call
//! that opens or ends. A list missing or left by a quit only keeps more folders.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ArchiveError;
use crate::settings::AppDirs;

#[derive(Serialize, Deserialize)]
struct OpenCalls {
    /// Folder names inside the calls folder.
    call_folder_names: Vec<String>,
}

/// The open-call list's file and the calls folder its names are in.
pub struct OpenCallsFile {
    path: PathBuf,
    calls_dir: PathBuf,
}

impl OpenCallsFile {
    #[must_use]
    pub fn new(app_dirs: &AppDirs) -> Self {
        Self {
            path: app_dirs.open_calls_file(),
            calls_dir: app_dirs.calls_dir(),
        }
    }

    /// Replaces the list in one rename, so a reader never sees half of it.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the file cannot be written.
    pub fn write(&self, call_folder_names: Vec<String>) -> std::io::Result<()> {
        let text = serde_json::to_vec_pretty(&OpenCalls { call_folder_names })
            .map_err(std::io::Error::other)?;
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut staged = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut staged, &text)?;
        staged.as_file().sync_all()?;
        staged.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }

    /// The open call folders. A missing list means none.
    ///
    /// # Errors
    ///
    /// Returns `ArchiveError` when the file cannot be read or is not a list.
    pub fn read(&self) -> Result<Vec<PathBuf>, ArchiveError> {
        let text = match std::fs::read(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ArchiveError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let open_calls: OpenCalls =
            serde_json::from_slice(&text).map_err(|error| ArchiveError::OpenCallsUnreadable {
                path: self.path.clone(),
                message: error.to_string(),
            })?;
        Ok(open_calls
            .call_folder_names
            .iter()
            .map(|name| self.calls_dir.join(name))
            .collect())
    }
}
