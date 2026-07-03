use keyring::v1::Entry;
use thiserror::Error;

const SERVICE: &str = "skillmon";
const USERNAME: &str = "anthropic-console-key";

#[derive(Debug, Error)]
#[error("API key store operation failed: {0}")]
pub struct ApiKeyStoreError(String);

/// The Console API key that unlocks exact `count_tokens` counts (ADR 0006),
/// stored in the OS keychain, never on disk (ADR 0020).
pub trait ApiKeyStore: Send {
    /// `None` covers both "no key configured" and "couldn't read the
    /// keychain" -- either way the caller falls back to the estimate tier.
    fn get(&self) -> Option<String>;
    /// Wired to the API-key settings command (set/forget key) in a later
    /// plan; only `get` is on the read path this plan exercises.
    #[allow(dead_code)]
    fn set(&self, key: &str) -> Result<(), ApiKeyStoreError>;
    #[allow(dead_code)]
    fn delete(&self) -> Result<(), ApiKeyStoreError>;
}

pub struct KeychainApiKeyStore {
    entry: Entry,
}

impl KeychainApiKeyStore {
    pub fn new() -> Result<Self, ApiKeyStoreError> {
        let entry = Entry::new(SERVICE, USERNAME).map_err(|e| ApiKeyStoreError(e.to_string()))?;
        Ok(Self { entry })
    }
}

impl ApiKeyStore for KeychainApiKeyStore {
    fn get(&self) -> Option<String> {
        self.entry.get_password().ok()
    }

    fn set(&self, key: &str) -> Result<(), ApiKeyStoreError> {
        self.entry.set_password(key).map_err(|e| ApiKeyStoreError(e.to_string()))
    }

    fn delete(&self) -> Result<(), ApiKeyStoreError> {
        self.entry.delete_credential().map_err(|e| ApiKeyStoreError(e.to_string()))
    }
}

/// In-memory stand-in used by every other module's tests -- the real OS
/// keychain is never exercised by the automated suite (see plan Task 6).
#[cfg(test)]
pub struct FakeApiKeyStore {
    key: std::cell::RefCell<Option<String>>,
}

#[cfg(test)]
impl FakeApiKeyStore {
    pub fn empty() -> Self {
        Self { key: std::cell::RefCell::new(None) }
    }

    pub fn with_key(key: &str) -> Self {
        Self { key: std::cell::RefCell::new(Some(key.to_string())) }
    }
}

#[cfg(test)]
impl ApiKeyStore for FakeApiKeyStore {
    fn get(&self) -> Option<String> {
        self.key.borrow().clone()
    }

    fn set(&self, key: &str) -> Result<(), ApiKeyStoreError> {
        *self.key.borrow_mut() = Some(key.to_string());
        Ok(())
    }

    fn delete(&self) -> Result<(), ApiKeyStoreError> {
        *self.key.borrow_mut() = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_store_starts_empty() {
        let store = FakeApiKeyStore::empty();
        assert_eq!(store.get(), None);
    }

    #[test]
    fn fake_store_round_trips_set_and_get() {
        let store = FakeApiKeyStore::empty();
        store.set("sk-ant-test-key").unwrap();
        assert_eq!(store.get(), Some("sk-ant-test-key".to_string()));
    }

    #[test]
    fn fake_store_delete_clears_the_key() {
        let store = FakeApiKeyStore::with_key("sk-ant-test-key");
        store.delete().unwrap();
        assert_eq!(store.get(), None);
    }

    /// Exercises the real OS keychain (ADR 0020). Not run by the default
    /// suite -- CI/sandboxed environments have no keychain to write to.
    /// Run by hand: `cargo test --manifest-path src-tauri/Cargo.toml
    /// footprint::api_key_store::tests::keychain_store_round_trips_against_the_real_os_keychain -- --ignored --exact`
    #[test]
    #[ignore]
    fn keychain_store_round_trips_against_the_real_os_keychain() {
        let store = KeychainApiKeyStore::new().expect("keychain entry should be constructible");

        // Clean slate in case a prior failed run left a key behind.
        let _ = store.delete();
        assert_eq!(store.get(), None, "expected no key before the test sets one");

        store.set("sk-ant-verification-probe").expect("set should succeed");
        assert_eq!(store.get(), Some("sk-ant-verification-probe".to_string()));

        store.delete().expect("delete should succeed");
        assert_eq!(store.get(), None, "expected no key after delete");
    }
}
