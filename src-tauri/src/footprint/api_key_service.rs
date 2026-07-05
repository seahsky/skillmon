use crate::footprint::api_key_store::ApiKeyStore;
use crate::footprint::count_tokens_client::{CountTokensClient, CountTokensError};
use serde::Serialize;

/// Outcome of trying to save an API key (issue #4). A rejected or unverified
/// key is a normal `Ok` outcome the UI renders, not an `Err`: `Err` is
/// reserved for a genuine keychain-write failure. This lets the panel tell a
/// mistyped key apart from a stored one, instead of silently leaving every
/// count an estimate (`compute::count_text` swallows a 401 and falls back to
/// the estimate tier with no signal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SetKeyOutcome {
    /// The probe succeeded, so the key is valid; stored.
    Stored,
    /// Stored, but the verifying probe couldn't reach Anthropic (offline,
    /// rate-limited, a 5xx, or a stale model id), so validity is unconfirmed.
    StoredUnverified,
    /// Anthropic rejected the key (401/403), or it was empty; not stored.
    Rejected,
}

/// Validate `key` with a single `count_tokens` probe *before* persisting it,
/// so a mistyped or revoked key is reported rather than silently falling
/// through to estimates. Probe-before-store guarantees a rejected key is never
/// written to the keychain. The key is never logged, and the returned `Err`
/// only ever carries the keychain layer's own message, never the key.
pub fn set_api_key(
    key: &str,
    store: &dyn ApiKeyStore,
    client: &dyn CountTokensClient,
    model_id: &str,
) -> Result<SetKeyOutcome, String> {
    let key = key.trim();
    if key.is_empty() {
        return Ok(SetKeyOutcome::Rejected);
    }

    match client.count_tokens("skillmon", model_id, key) {
        Ok(_) => {
            store.set(key).map_err(|e| e.to_string())?;
            Ok(SetKeyOutcome::Stored)
        }
        // An explicit auth rejection: the key is bad, so never store it.
        Err(CountTokensError::UnexpectedStatus { status, .. }) if status == 401 || status == 403 => {
            Ok(SetKeyOutcome::Rejected)
        }
        // Network, rate-limit, 5xx, or malformed response: can't confirm, but
        // don't block an offline user from saving a key they believe is good.
        Err(_) => {
            store.set(key).map_err(|e| e.to_string())?;
            Ok(SetKeyOutcome::StoredUnverified)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::api_key_store::FakeApiKeyStore;
    use crate::footprint::count_tokens_client::FakeCountTokensClient;

    const MODEL: &str = "claude-sonnet-5";

    #[test]
    fn a_valid_key_is_probed_then_stored() {
        let store = FakeApiKeyStore::empty();
        let client = FakeCountTokensClient::always_returns(5);

        let outcome = set_api_key("sk-ant-good", &store, &client, MODEL).unwrap();

        assert_eq!(outcome, SetKeyOutcome::Stored);
        assert_eq!(store.get(), Some("sk-ant-good".to_string()));
        assert_eq!(client.call_count(), 1, "a valid save probes exactly once");
    }

    #[test]
    fn an_auth_rejected_key_is_not_stored() {
        let store = FakeApiKeyStore::empty();
        let client = FakeCountTokensClient::always_rejects_unauthorized();

        let outcome = set_api_key("sk-ant-bad", &store, &client, MODEL).unwrap();

        assert_eq!(outcome, SetKeyOutcome::Rejected);
        assert_eq!(store.get(), None, "a rejected key must never reach the keychain");
    }

    #[test]
    fn a_transient_error_stores_but_reports_unverified() {
        let store = FakeApiKeyStore::empty();
        let client = FakeCountTokensClient::always_fails();

        let outcome = set_api_key("sk-ant-maybe", &store, &client, MODEL).unwrap();

        assert_eq!(outcome, SetKeyOutcome::StoredUnverified);
        assert_eq!(store.get(), Some("sk-ant-maybe".to_string()), "offline should not block saving");
    }

    #[test]
    fn an_empty_or_whitespace_key_is_rejected_without_probing_or_storing() {
        let store = FakeApiKeyStore::empty();
        let client = FakeCountTokensClient::always_returns(5);

        let outcome = set_api_key("   ", &store, &client, MODEL).unwrap();

        assert_eq!(outcome, SetKeyOutcome::Rejected);
        assert_eq!(store.get(), None);
        assert_eq!(client.call_count(), 0, "an empty key must not hit the network");
    }

    #[test]
    fn a_keychain_write_failure_never_leaks_the_key_in_the_error() {
        let store = FakeApiKeyStore::failing_to_write();
        let client = FakeCountTokensClient::always_returns(5);

        let err = set_api_key("sk-ant-SECRET-XYZ", &store, &client, MODEL).unwrap_err();

        assert!(!err.contains("SECRET-XYZ"), "the key must never appear in an error string, got: {err}");
    }
}
