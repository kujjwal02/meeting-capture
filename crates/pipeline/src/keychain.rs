//! This install's secrets in the macOS Keychain or Windows Credential Manager.

use crate::settings::ProviderKind;

// The reason is text: some keyring errors carry the raw secret bytes in their Debug output.
#[derive(Debug, thiserror::Error)]
#[error("could not {action} in the OS keychain: {reason}")]
pub struct KeychainError {
    action: &'static str,
    reason: String,
}

/// This install's entries in the macOS Keychain or Windows Credential Manager.
#[derive(Debug, Clone)]
pub struct Keychain {
    service: String,
}

impl Keychain {
    #[must_use]
    pub fn new(service: String) -> Self {
        Self { service }
    }

    pub(crate) fn read(&self, user: &str) -> Result<Option<String>, KeychainError> {
        let keychain_error = |source: keyring::Error| KeychainError {
            action: "read a secret",
            reason: source.to_string(),
        };
        match keyring::Entry::new(&self.service, user)
            .map_err(keychain_error)?
            .get_password()
        {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(source) => Err(keychain_error(source)),
        }
    }

    pub(crate) fn write(&self, user: &str, secret: &str) -> Result<(), KeychainError> {
        let keychain_error = |source: keyring::Error| KeychainError {
            action: "save a secret",
            reason: source.to_string(),
        };
        keyring::Entry::new(&self.service, user)
            .map_err(keychain_error)?
            .set_password(secret)
            .map_err(keychain_error)
    }

    pub(crate) fn delete(&self, user: &str) -> Result<(), KeychainError> {
        let keychain_error = |source: keyring::Error| KeychainError {
            action: "delete a secret",
            reason: source.to_string(),
        };
        match keyring::Entry::new(&self.service, user)
            .map_err(keychain_error)?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(source) => Err(keychain_error(source)),
        }
    }

    /// The API key saved for an LLM provider, if any.
    ///
    /// # Errors
    ///
    /// Returns `KeychainError` when the keychain refuses the read.
    pub fn llm_api_key(&self, provider: ProviderKind) -> Result<Option<String>, KeychainError> {
        self.read(&llm_api_key_user(provider))
    }

    /// # Errors
    ///
    /// Returns `KeychainError` when the keychain refuses the write.
    pub fn set_llm_api_key(
        &self,
        provider: ProviderKind,
        api_key: &str,
    ) -> Result<(), KeychainError> {
        self.write(&llm_api_key_user(provider), api_key)
    }

    /// # Errors
    ///
    /// Returns `KeychainError` when the keychain refuses the delete.
    pub fn delete_llm_api_key(&self, provider: ProviderKind) -> Result<(), KeychainError> {
        self.delete(&llm_api_key_user(provider))
    }
}

fn llm_api_key_user(provider: ProviderKind) -> String {
    format!("llm-api-key:{}", provider.as_str())
}
