use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError, RwLock};

use super::{AppDirs, LlmSettings, Settings, SettingsError, SettingsFile, SlackSettings};
use crate::calendar::GoogleAccounts;

/// The settings file and the sections that apply while the app runs: LLM, Google and Slack.
/// Every edit of the file goes through `edit`, which also hands the new sections to their
/// readers.
pub struct SettingsStore {
    app_dirs: AppDirs,
    /// Held across each read, edit and write of the file and while the live sections take the
    /// new values, so two edits never drop each other's change and the live sections match the
    /// last write. It guards this process only; `mc` never writes the file.
    file_lock: Mutex<()>,
    llm: RwLock<LlmSettings>,
    slack: RwLock<SlackSettings>,
    google: GoogleAccounts,
}

/// What an edit changed that the caller acts on.
#[must_use = "revoke tokens_to_revoke, or Google keeps the grants"]
pub struct SettingsChange {
    /// The edit dropped the API key, such as the key of the old endpoint.
    pub has_dropped_llm_api_key: bool,
    /// The edit dropped the client secret of the old Google client.
    pub has_dropped_google_client_secret: bool,
    /// Refresh tokens the edit dropped, by email. Google still honours them until revoked.
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
            slack: RwLock::new(SlackSettings::default()),
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
    /// Google and Slack sections to their readers, all under one lock.
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
        file.write(&self.app_dirs)?;
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

    /// The huddle watcher reads these before each step, so a change applies to the next huddle.
    #[must_use]
    pub fn slack(&self) -> SlackSettings {
        *self.slack.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The signed-in Google accounts and whether each still works.
    #[must_use]
    pub fn google(&self) -> &GoogleAccounts {
        &self.google
    }

    fn take_live_sections(&self, file: &SettingsFile) {
        *self.llm.write().unwrap_or_else(PoisonError::into_inner) = file.llm.clone();
        *self.slack.write().unwrap_or_else(PoisonError::into_inner) = file.slack;
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
        let tokens_to_revoke = old
            .google
            .refresh_tokens
            .iter()
            .filter(|(email, _)| !new.google.refresh_tokens.contains_key(*email))
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
