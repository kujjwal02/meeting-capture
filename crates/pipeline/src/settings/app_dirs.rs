use std::path::PathBuf;

use directories::ProjectDirs;

use super::SettingsError;

/// Where the app keeps its own files. None of them go into the vault.
#[derive(Debug, Clone)]
pub struct AppDirs {
    pub(super) config_dir: PathBuf,
    data_dir: PathBuf,
}

impl AppDirs {
    /// The per-user folders named after the Tauri app identifier. `MEETING_CAPTURE_HOME`, when
    /// set, holds both instead, so a dev run never touches the real job store.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError::NoHomeDir` when the OS reports no home folder.
    pub fn locate() -> Result<Self, SettingsError> {
        if let Some(home) = std::env::var_os("MEETING_CAPTURE_HOME") {
            let home = PathBuf::from(home);
            return Ok(Self {
                config_dir: home.join("config"),
                data_dir: home.join("data"),
            });
        }
        let project = ProjectDirs::from("com", "ujjwalkumar", "meetingcapture")
            .ok_or(SettingsError::NoHomeDir)?;
        Ok(Self {
            config_dir: project.config_dir().to_path_buf(),
            data_dir: project.data_dir().to_path_buf(),
        })
    }

    /// Creates the folders the pipeline writes into.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError::CreateDir` naming the folder that failed.
    pub fn create(&self) -> Result<(), SettingsError> {
        for path in [
            &self.config_dir,
            &self.payload_dir(),
            &self.huddle_log_dir(),
            &self.note_staging_dir(),
            &self.log_dir(),
        ] {
            std::fs::create_dir_all(path).map_err(|source| SettingsError::CreateDir {
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }

    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }

    /// Overrides the built-in note template when present.
    #[must_use]
    pub fn note_template_file(&self) -> PathBuf {
        self.config_dir.join("note.md.j2")
    }

    #[must_use]
    pub fn job_store_file(&self) -> PathBuf {
        self.data_dir.join("jobs.sqlite")
    }

    /// Raw inputs: TranscripTonic payloads as received.
    #[must_use]
    pub fn payload_dir(&self) -> PathBuf {
        self.data_dir.join("payloads")
    }

    /// Raw inputs: one log per Slack huddle, as the huddle watcher wrote it.
    #[must_use]
    pub fn huddle_log_dir(&self) -> PathBuf {
        self.data_dir.join("huddles")
    }

    /// Layout profiles that override the built-in ones, for a Slack release the app does not
    /// know yet.
    #[must_use]
    pub fn slack_layout_dir(&self) -> PathBuf {
        self.config_dir.join("slack-layouts")
    }

    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// The empty working folder for Claude Code, so it reads no project files.
    #[must_use]
    pub fn claude_code_dir(&self) -> PathBuf {
        self.data_dir.join("claude-code")
    }

    /// Notes are written here first, then renamed into the vault, so Syncthing never sees a
    /// half-written note.
    #[must_use]
    pub fn note_staging_dir(&self) -> PathBuf {
        self.data_dir.join("note-staging")
    }
}
