use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

use super::{
    AppDirs, DriveSettings, LlmSettings, MeetSettings, Settings, SettingsError, SettingsFile,
    SlackSettings, TranscriptionSettings,
};
use crate::calendar::GoogleAccounts;

/// The settings file and the sections that apply while the app runs: LLM, transcription, Google,
/// Drive, Slack and Meet.
/// Every edit of the file goes through `edit`, which also hands the new sections to their
/// readers.
pub struct SettingsStore {
    app_dirs: AppDirs,
    /// Held across each read, edit and write of the file and while the live sections take the
    /// new values, so two edits never drop each other's change and the live sections match the
    /// last write. It guards this process only; `mc` writes the file only for `mc drive sign-in`.
    file_lock: Mutex<()>,
    llm: RwLock<LlmSettings>,
    transcription: RwLock<TranscriptionSettings>,
    slack: RwLock<SlackSettings>,
    meet: RwLock<MeetSettings>,
    drive: RwLock<DriveSettings>,
    google: GoogleAccounts,
}

/// What an edit changed that the caller acts on.
#[must_use = "revoke tokens_to_revoke, or Google keeps the grants"]
pub struct SettingsChange {
    /// The edit dropped the API key, such as the key of the old endpoint.
    pub has_dropped_llm_api_key: bool,
    /// The edit dropped the client secret of the old Google client.
    pub has_dropped_google_client_secret: bool,
    /// Refresh tokens the edit dropped, calendar and Drive, by email, whose account has no token
    /// left. Google still honours them until revoked. A dropped token whose account keeps another
    /// token is left out: Google keeps one grant per account and client, so its revoke would end
    /// the kept token too.
    pub tokens_to_revoke: BTreeMap<String, String>,
    /// Every account must sign in again: Google issued their grants to the old client.
    pub has_new_google_client: bool,
}

impl SettingsStore {
    /// A store whose live sections stay empty until `load` or `edit` succeeds.
    #[must_use]
    pub fn new(app_dirs: AppDirs) -> Self {
        Self {
            app_dirs,
            file_lock: Mutex::new(()),
            llm: RwLock::new(LlmSettings::default()),
            transcription: RwLock::new(TranscriptionSettings::default()),
            slack: RwLock::new(SlackSettings::default()),
            meet: RwLock::new(MeetSettings::default()),
            drive: RwLock::new(DriveSettings::default()),
            google: GoogleAccounts::default(),
        }
    }

    #[must_use]
    pub fn app_dirs(&self) -> &AppDirs {
        &self.app_dirs
    }

    /// Loads and validates the settings file, and takes its live sections. A missing file gives
    /// the defaults.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the file or the invalid field.
    pub(crate) fn load(&self) -> Result<Settings, SettingsError> {
        let _file_guard = self.lock_file();
        let file = SettingsFile::read(&self.app_dirs)?;
        let settings = Settings::resolve(&file)?;
        self.take_live_sections(&file);
        Ok(settings)
    }

    /// The file as written, for showing it. Edit it with `edit`.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the file.
    pub fn read(&self) -> Result<SettingsFile, SettingsError> {
        SettingsFile::read(&self.app_dirs)
    }

    /// Reads the file, applies `edit`, validates and writes the result, and hands the LLM,
    /// Google, Slack and Meet sections to their readers, all under one lock.
    ///
    /// # Errors
    ///
    /// Returns the error from `edit`, or `SettingsError` naming the file or the invalid field.
    /// The file and the live sections stay as they were.
    pub fn edit(
        &self,
        edit: impl FnOnce(&mut SettingsFile) -> Result<(), SettingsError>,
    ) -> Result<SettingsChange, SettingsError> {
        let _file_guard = self.lock_file();
        let old = SettingsFile::read(&self.app_dirs)?;
        let mut file = old.clone();
        edit(&mut file)?;
        file.write(&self.app_dirs, &old)?;
        self.take_live_sections(&file);
        Ok(SettingsChange::between(&old, &file))
    }

    /// The LLM settings the next extraction uses.
    #[must_use]
    pub fn llm(&self) -> LlmSettings {
        self.llm
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The speech-to-text settings the next transcribe step uses.
    #[must_use]
    pub fn transcription(&self) -> TranscriptionSettings {
        self.transcription
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The huddle watcher reads these before each step, so a change applies to the next huddle.
    #[must_use]
    pub fn slack(&self) -> SlackSettings {
        *self.slack.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// mc-meet gets these when it connects and after each edit.
    #[must_use]
    pub fn meet(&self) -> MeetSettings {
        *self.meet.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The Drive account the next upload or download uses.
    #[must_use]
    pub fn drive(&self) -> DriveSettings {
        self.drive
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Reads `[drive]` from the file again and takes it as the live section. `mc drive sign-in`
    /// writes the file from another process, which this store's lock does not cover, so a
    /// long-running reader calls this before each run instead of trusting the copy from the last
    /// load or edit.
    ///
    /// # Errors
    ///
    /// Returns `SettingsError` naming the file or the invalid `drive` field. The live section
    /// stays as it was.
    pub fn reload_drive(&self) -> Result<DriveSettings, SettingsError> {
        let _file_guard = self.lock_file();
        let drive = SettingsFile::read(&self.app_dirs)?.drive;
        drive.validate()?;
        drive.clone_into(&mut self.drive.write().unwrap_or_else(PoisonError::into_inner));
        Ok(drive)
    }

    /// The signed-in Google accounts and whether each still works.
    #[must_use]
    pub fn google(&self) -> &GoogleAccounts {
        &self.google
    }

    fn take_live_sections(&self, file: &SettingsFile) {
        *self.llm.write().unwrap_or_else(PoisonError::into_inner) = file.llm.clone();
        // A `[debug]`-only edit validates that table alone, so it can carry a hand edit of
        // `[transcription]` that never passed validation.
        match file.transcription.validate() {
            Ok(()) => {
                *self
                    .transcription
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = file.transcription.clone();
            }
            Err(error) => tracing::warn!(
                exception.message = %error,
                "transcription settings invalid; the previous ones stay in use"
            ),
        }
        *self.slack.write().unwrap_or_else(PoisonError::into_inner) = file.slack;
        *self.meet.write().unwrap_or_else(PoisonError::into_inner) = file.meet;
        // Like `[transcription]`, a `[debug]`-only edit can carry a hand edit that never passed
        // validation.
        match file.drive.validate() {
            Ok(()) => {
                *self.drive.write().unwrap_or_else(PoisonError::into_inner) = file.drive.clone();
            }
            Err(error) => tracing::warn!(
                exception.message = %error,
                "drive settings invalid; the previous ones stay in use"
            ),
        }
        self.google.replace(&file.google);
    }

    fn lock_file(&self) -> MutexGuard<'_, ()> {
        self.file_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl SettingsChange {
    fn between(old: &SettingsFile, new: &SettingsFile) -> Self {
        // A token replaced by a new sign-in is not revoked: Google revokes the whole grant, so
        // the new token would stop working too.
        let dropped_drive_token = old
            .drive
            .account
            .as_ref()
            .zip(old.drive.refresh_token.as_ref());
        let tokens_to_revoke = old
            .google
            .refresh_tokens
            .iter()
            .chain(dropped_drive_token)
            .filter(|(email, _)| !new.has_token_for(email))
            .map(|(email, refresh_token)| (email.clone(), refresh_token.clone()))
            .collect();
        Self {
            has_dropped_llm_api_key: old.llm.api_key.is_some() && new.llm.api_key.is_none(),
            has_dropped_google_client_secret: old.google.client_secret.is_some()
                && new.google.client_secret.is_none(),
            tokens_to_revoke,
            has_new_google_client: old.google.client_id() != new.google.client_id(),
        }
    }
}
