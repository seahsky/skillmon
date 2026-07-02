# 20. Console API key lives in the OS keychain, never on disk in plaintext

## Status

Accepted.

## Context

ADR 0006 lets a user paste their own Anthropic Console `ANTHROPIC_API_KEY` into skillmon to unlock exact `count_tokens` counts.
That key is a billable secret — anyone who reads it off disk can spend the user's money — so where skillmon persists it is a security decision, not an implementation detail, and one that's expensive to revisit once users have onboarded around it.

## Decision

Store the key via the `keyring` crate's `v1::Entry` API, which resolves to Keychain Services on macOS and Credential Manager on Windows.
Never write the key into SQLite, `tauri-plugin-store`, or any other on-disk file, even encrypted.
skillmon reads it once per process (or on-demand before a cache-miss call) and holds it in memory only for the duration of the call.

## Consequences

- No bespoke crypto to write or audit — the platform keychain is the security boundary, and `keyring`'s default (`v1`) feature already pulls in the right native backend per OS (`apple-native-keyring-store`/`keychain` on macOS, `windows-native-keyring-store` on Windows) with no extra feature flags needed for this app's target platforms.
- Deleting the key (e.g. a "forget my key" action) is a single `delete_credential()` call, not a file-scrub.
- Every keychain access can prompt the OS's own permission UI on first use; the API-key-entry flow in the UI must expect and explain that prompt rather than treat it as a failure.
- Tests can't exercise the real OS keychain in CI, so the key-store is accessed through a small trait skillmon owns, with a real `keyring`-backed implementation and an in-memory fake for tests.

## Options considered

- **Plaintext in SQLite settings table** — simplest, but leaves a billable secret in cleartext on disk; rejected.
- **App-managed encrypted file** — avoids keychain API surface, but is a bespoke crypto implementation to get right (key derivation, at-rest threat model) for no benefit over a battle-tested OS facility; rejected.
- **OS keychain via `keyring`** — chosen.
