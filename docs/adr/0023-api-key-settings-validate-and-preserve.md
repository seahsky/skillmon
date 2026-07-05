# 23. API-key settings: validate on save, keep exact counts on remove

## Status

Accepted.

## Context

ADR 0006 makes exact counts available only when the user supplies their own Console API key, stored in the OS keychain (ADR 0020).
Issue #4 wires the settings surface and the set/delete commands that were built but unused.
Three forces shaped the design beyond "wrap set/delete":

- A keychain write can block on a modal OS authorization dialog for an unbounded time, and the first scan after a key is added makes a `count_tokens` call per uncached layer, which can run for many seconds.
- `compute::count_text` swallows a `count_tokens` failure (including a 401 for a bad key) and falls back to the estimate tier with no signal, so a mistyped key would silently leave every count an estimate.
- `count_text` returns a cached exact value before it ever checks for a key, so an exact count already computed stays valid regardless of whether a key is currently present.

## Decision

1. Manage the key store separately from the adapter.
The `set_api_key` and `delete_api_key` commands lock their own `Arc<Mutex<ApiKeySettings>>`, never the adapter's scan mutex, so a keychain prompt can never freeze an in-flight scan and a slow scan can never freeze a Save.
The settings store is a second stateless handle over the same keychain entry the adapter reads, so the two stay coherent.

2. Validate the key on save, before storing it.
`set_api_key` probes `count_tokens` once with the not-yet-stored key and returns a `SetKeyOutcome`: `Stored` on success, `Rejected` on a 401/403 (the key is never stored), and `StoredUnverified` on a network or server error (stored anyway, so an offline user isn't blocked).
A rejected or unverified key is a normal outcome the UI renders, not an error; `Err` is reserved for a genuine keychain-write failure and never contains the key.

3. Removing a key does not purge already-computed exact counts.
`delete_api_key` clears only the keychain; the cached exact counts are still true (the skill text is unchanged and `count_tokens` is deterministic per content hash and model), so throwing them away would be strictly worse information.
Remove stops new exact counts; existing ones stay exact until their skill content changes, and the copy says exactly that.

4. Key-presence rides in the scan payload.
`ScanReport` carries `api_key_present` (a bool, never the key), set from the adapter via a `HarnessAdapter::api_key_present()` method, so the settings state, the legend, and the exact/estimate badges all flip from one `list_skills` fetch with no second command and no state-sync race.

## Consequences

- The key never crosses back to the webview, is never logged, and never appears in an error string; only its presence (a bool) and the save outcome are exposed.
- A wrong key gets immediate, distinct feedback instead of a silent all-estimate result.
- The first-key scan is slow (a sequential `count_tokens` burst); the panel shows a distinct "counting for the first time" banner so it doesn't read as hung, but the latency itself is the on-demand read and network fan-out tracked under issues #2/#3, not fixed here.
- The settings surface is a full-panel view-swap behind a topbar gear, chosen over an inline section (which would reflow the fixed skill-row grid) or a modal (heavy on a compact tray panel).

## Options considered

- **Delegate set/delete through the adapter's private store**: rejected, because it serializes key writes behind the scan mutex for no coherence benefit (the keychain store is stateless).
- **A `has_api_key` command**: rejected in favor of `api_key_present` in `ScanReport`, one source of truth that flips with the badges.
- **Accept a key blindly without validating**: rejected, because `compute::count_text`'s silent 401 fallback would leave the user with no idea a key was wrong.
- **Purge the exact cache on remove to force re-estimation**: rejected as strictly worse information; the cached exacts are still true.
