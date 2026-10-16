//! The settings file and the app's folders. The tray app and `mc` both load settings here.

use std::path::{Path, PathBuf};

use directories::{ProjectDirs, UserDirs};
use jiff::tz::TimeZone;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("the OS reports no home folder, so the app folders cannot be placed")]
    NoHomeDir,
    #[error("could not read settings file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("settings file {path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("settings field vault_dir: {path} is not a folder")]
    VaultDirMissing { path: PathBuf },
    #[error("settings field my_name: must not be blank")]
    BlankMyName,
    #[error(
        "settings field time_zone: {name:?} is not a time zone; use an IANA name such as \"Asia/Kolkata\" or \"Europe/London\""
    )]
    UnknownTimeZone { name: String },
    #[error(
        "settings field time_zone: the device time zone is unknown ({reason}); set an IANA name such as \"Asia/Kolkata\""
    )]
    NoDeviceTimeZone { reason: String },
    #[error("could not create app folder {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Where the app keeps its own files. None of them go into the vault.
#[derive(Debug, Clone)]
pub struct AppDirs {
    config_dir: PathBuf,
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

    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// Notes are written here first, then renamed into the vault, so Syncthing never sees a
    /// half-written note.
    #[must_use]
    pub fn note_staging_dir(&self) -> PathBuf {
        self.data_dir.join("note-staging")
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub vault_dir: PathBuf,
    /// Replaces "You", the name TranscripTonic gives the local user.
    pub my_name: Option<String>,
    pub receiver_port: u16,
    /// Note names, dates and the times the tray and `mc` show. Stored times stay UTC.
    pub time_zone: TimeZone,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    vault_dir: Option<PathBuf>,
    my_name: Option<String>,
    receiver_port: Option<u16>,
    time_zone: Option<String>,
}

const DEFAULT_RECEIVER_PORT: u16 = 47615;

/// Written when the user opens a settings file that does not exist yet.
pub const SETTINGS_TEMPLATE: &str = "\
# Meeting Capture settings. Quit and reopen the app after a change.
# Uncomment a line to change it from the default.

# The Obsidian vault. Notes go under areas/meetings.
# vault_dir = \"/Users/you/knowledge\"

# Replaces \"You\" in TranscripTonic transcripts.
# my_name = \"Your Name\"

# The port in TranscripTonic's webhook URL, http://127.0.0.1:47615/transcriptonic.
# receiver_port = 47615

# The IANA time zone for note names and shown times. Defaults to the device's time zone.
# time_zone = \"Asia/Kolkata\"
";

impl Settings {
    /// Loads and validates the settings file. A missing file gives the defaults.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the file or the invalid field.
    pub fn load(app_dirs: &AppDirs) -> Result<Self, SettingsError> {
        let path = app_dirs.settings_file();
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|source| SettingsError::Parse {
                path: path.clone(),
                source,
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SettingsFile {
                vault_dir: None,
                my_name: None,
                receiver_port: None,
                time_zone: None,
            },
            Err(source) => return Err(SettingsError::Read { path, source }),
        };

        let vault_dir = match file.vault_dir {
            Some(vault_dir) => vault_dir,
            None => UserDirs::new()
                .ok_or(SettingsError::NoHomeDir)?
                .home_dir()
                .join("knowledge"),
        };
        if !vault_dir.is_dir() {
            return Err(SettingsError::VaultDirMissing { path: vault_dir });
        }
        if file
            .my_name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(SettingsError::BlankMyName);
        }
        let time_zone = match file.time_zone {
            Some(name) => {
                TimeZone::get(&name).map_err(|_| SettingsError::UnknownTimeZone { name })?
            }
            None => TimeZone::try_system().map_err(|error| SettingsError::NoDeviceTimeZone {
                reason: error.to_string(),
            })?,
        };
        Ok(Self {
            vault_dir,
            my_name: file.my_name,
            receiver_port: file.receiver_port.unwrap_or(DEFAULT_RECEIVER_PORT),
            time_zone,
        })
    }

    #[must_use]
    pub fn meetings_dir(&self) -> PathBuf {
        self.vault_dir.join(Path::new("areas").join("meetings"))
    }
}
