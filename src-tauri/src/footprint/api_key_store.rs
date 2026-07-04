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
    /// Persist the key to the OS keychain. Driven by the `set_api_key`
    /// command (issue #4), which validates the key with a `count_tokens`
    /// probe before calling this, so a rejected key is never written.
    fn set(&self, key: &str) -> Result<(), ApiKeyStoreError>;
    /// Forget the key. Idempotent: removing an already-absent key succeeds,
    /// since "no key" is the desired end state either way.
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
        match self.entry.delete_credential() {
            Ok(()) => Ok(()),
            // Already absent is the desired end state, so a Remove never
            // surfaces a spurious failure for a key that isn't there.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(ApiKeyStoreError(e.to_string())),
        }
    }
}

/// In-memory stand-in used by every other module's tests -- the real OS
/// keychain is never exercised by the automated suite. The `#[ignore]`d
/// round-trip test below is the only real-keychain exercise, run by hand
/// (ADR 0020, issue #4).
#[cfg(test)]
pub struct FakeApiKeyStore {
    key: std::cell::RefCell<Option<String>>,
    fail_write: bool,
}

#[cfg(test)]
impl FakeApiKeyStore {
    pub fn empty() -> Self {
        Self { key: std::cell::RefCell::new(None), fail_write: false }
    }

    pub fn with_key(key: &str) -> Self {
        Self { key: std::cell::RefCell::new(Some(key.to_string())), fail_write: false }
    }

    /// A store whose `set` always fails, to prove the failure path never leaks
    /// the key in its error (issue #4).
    pub fn failing_to_write() -> Self {
        Self { key: std::cell::RefCell::new(None), fail_write: true }
    }
}

#[cfg(test)]
impl ApiKeyStore for FakeApiKeyStore {
    fn get(&self) -> Option<String> {
        self.key.borrow().clone()
    }

    fn set(&self, key: &str) -> Result<(), ApiKeyStoreError> {
        if self.fail_write {
            // Deliberately does not include the key value.
            return Err(ApiKeyStoreError("keychain write failed".to_string()));
        }
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
