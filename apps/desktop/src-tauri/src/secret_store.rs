//! TASK-908: API credentials live in the operating-system credential vault only.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecretStoreError {
    Unavailable,
    Missing,
    Rejected,
}

pub(crate) trait SecretStore: Send + Sync {
    fn set_api_key(&self, value: &str) -> Result<(), SecretStoreError>;
    fn get_api_key(&self) -> Result<String, SecretStoreError>;
    fn delete_api_key(&self) -> Result<(), SecretStoreError>;

    fn has_api_key(&self) -> Result<bool, SecretStoreError> {
        match self.get_api_key() {
            Ok(_) => Ok(true),
            Err(SecretStoreError::Missing) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

#[cfg(windows)]
pub(crate) struct SystemSecretStore;

#[cfg(windows)]
impl SystemSecretStore {
    const ACCOUNT: &'static str = "provider-api-key";
    const SERVICE: &'static str = "com.bigdatadz.ideal-harness";

    fn entry() -> Result<keyring::Entry, SecretStoreError> {
        keyring::Entry::new(Self::SERVICE, Self::ACCOUNT).map_err(|_| SecretStoreError::Unavailable)
    }
}

#[cfg(windows)]
impl SecretStore for SystemSecretStore {
    fn set_api_key(&self, value: &str) -> Result<(), SecretStoreError> {
        if value.trim().is_empty() {
            return Err(SecretStoreError::Rejected);
        }
        Self::entry()?
            .set_password(value)
            .map_err(|_| SecretStoreError::Unavailable)
    }

    fn get_api_key(&self) -> Result<String, SecretStoreError> {
        Self::entry()?.get_password().map_err(|error| match error {
            keyring::Error::NoEntry => SecretStoreError::Missing,
            _ => SecretStoreError::Unavailable,
        })
    }

    fn delete_api_key(&self) -> Result<(), SecretStoreError> {
        Self::entry()?
            .delete_credential()
            .map_err(|error| match error {
                keyring::Error::NoEntry => SecretStoreError::Missing,
                _ => SecretStoreError::Unavailable,
            })
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn windows_credential_round_trip_and_delete_are_effective() {
        let account = format!("task-908-test-{}", std::process::id());
        let entry = keyring::Entry::new("com.bigdatadz.ideal-harness.test", &account).unwrap();
        let _ = entry.delete_credential();
        entry.set_password("ephemeral-test-secret").unwrap();
        assert_eq!(entry.get_password().unwrap(), "ephemeral-test-secret");
        entry.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));
    }
}

#[cfg(not(windows))]
pub(crate) struct SystemSecretStore;

#[cfg(not(windows))]
impl SecretStore for SystemSecretStore {
    fn set_api_key(&self, _value: &str) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Unavailable)
    }

    fn get_api_key(&self) -> Result<String, SecretStoreError> {
        Err(SecretStoreError::Unavailable)
    }

    fn delete_api_key(&self) -> Result<(), SecretStoreError> {
        Err(SecretStoreError::Unavailable)
    }
}
