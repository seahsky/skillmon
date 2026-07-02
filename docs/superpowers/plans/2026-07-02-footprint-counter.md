# Footprint Counter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Compute the three-layer context footprint (always-on, on-invoke, on-demand) for a `DiscoveredSkill`, tagged with token source (exact/estimate) and — for always-on only — text confidence (native/reconstructed), backed by a content-addressed SQLite token cache and an optional exact `count_tokens` path gated on a keychain-stored API key. This is the direct follow-up to `2026-07-02-skill-discovery.md`, which explicitly deferred all of this.

**Architecture:** Generic, harness-agnostic pieces (hashing, cache, tokenizer, calibration, API key store, HTTP client, orchestration) live under a new `src-tauri/src/footprint/` module — none of it is Claude-Code-specific, so per ADR 0002 it does not belong under `adapters::claude_code`. The one Claude-Code-specific piece — *which literal text* each layer measures (transcript-bullet extraction, the on-invoke template, on-demand file enumeration) — lives in a new `adapters::claude_code::footprint_text` module. `ClaudeCodeAdapter` gains a `compute_footprint` method that sources text from `footprint_text` and counts it via `footprint::compute`. A `HarnessAdapter` trait is introduced now (deferred by the prior plan until both methods existed) with `discover_skills` and `compute_footprint`.

**Tech stack additions:** `tiktoken-rs` 0.12 (`o200k_base`), `sha2` 0.11, `rusqlite` 0.40 (`bundled`), `reqwest` 0.13 (`blocking`, `json`), `keyring` 4 (default `v1` feature — already pulls in `apple-native-keyring-store/keychain` and `windows-native-keyring-store`, no extra flags needed). Dev-only: `mockito` 1 for HTTP-call tests.

Grilling this plan surfaced five implementation-level decisions the existing ADRs didn't pin down; the reasoning for each is recorded once here rather than duplicated per task:

1. **Cache shape** — one row per `content_hash` (not per `(hash, model_id)` compound key). ADR 0006's compound key and ADR 0018's "reference model is a single fixed internal default" together mean there is only ever one live exact model at a time, so the row just carries `exact_model_id` as a column checked for staleness, alongside an always-present `tiktoken_count` column used for both the raw estimate and calibration math. This also means the *same* row serves both tiers — no duplicate tiktoken-only rows to keep in sync.
2. **On-demand hashing is per-file**, not per-skill-blob: each bundled reference file is its own cache row, summed as the ceiling. Shared reference files across skills (and partial edits) reuse/invalidate correctly for free under the content-addressed model.
3. **API key storage is the OS keychain** (ADR 0020), accessed through a small `ApiKeyStore` trait so tests use a fake instead of touching the real OS keychain.
4. **HTTP stays synchronous** (`reqwest::blocking`), matching ADR 0008's sync-core philosophy — no tokio in `footprint::compute`. Callers on an async Tauri command boundary wrap it in `spawn_blocking`; that wrapping is out of scope here since no Tauri commands exist yet for footprint (follow-up).
5. **First-run bulk misses are sequential**, not concurrent. `count_tokens` is only ever called on a cache miss, steady state is offline, and a first-run cost of one blocking call per changed skill is an accepted simplification (YAGNI) — revisit only if it proves slow in practice.
6. **A failed `count_tokens` call and "no key configured" share one fallback path** — both mean "exact isn't available right now," so both fall through to the calibrated-estimate branch with `TokenSource::Estimate`. No third state, no retries.
7. **Always-on transcript search scope is skill-type-dependent**: personal and user-scoped-plugin skills search across *all* known repos' transcripts (most-recently-modified first, reusing `discovery::transcript::enumerate_known_repos`), since those skills can appear in any repo's session; project skills and project/local-scoped plugin skills search only their own repo's transcripts, since that's the only place they can ever render.

## Global Constraints

- Everything under `src-tauri/src/footprint/` is harness-agnostic — no `~/.claude` paths, no transcript JSON shapes, nothing Claude-Code-specific (ADR 0002). If a future second harness adapter needs footprint counting, it reuses this module untouched.
- `footprint::compute` and everything it calls is synchronous. No `async fn`, no tokio, anywhere in `footprint/` or `domain/` (ADR 0008's precedent, extended here).
- No network call happens except inside `footprint::count_tokens_client`, and only on an explicit cache miss with a configured key.
- The Console API key is never logged, never written to SQLite, never included in a `Debug` derive's output verbatim (wrap it in a newtype with a redacted `Debug` impl).
- Every new SQLite table lives in the same database file the discovery/attribution work will eventually share (ADR 0008); this plan creates the file and its own tables, additive to whatever comes later.
- `model_id` / reference model is never surfaced to the UI (ADR 0018) — nothing in this plan adds a model picker or model label to any public return type the UI would render directly.

---

## File Structure

```
src-tauri/src/
  lib.rs                                    (modify: add `mod footprint;`)
  Cargo.toml                                 (modify: new deps)
  domain/
    mod.rs                                  (modify: `pub mod footprint;` `pub mod harness;`)
    footprint.rs                            (new: TokenSource, TextConfidence, LayerCount, AlwaysOnFootprint, Footprint)
    harness.rs                              (new: HarnessAdapter trait)
  footprint/
    mod.rs                                  (new: module wiring)
    hashing.rs                              (new: sha256_hex)
    cache.rs                                (new: TokenCache — rusqlite-backed, content-addressed + calibration table)
    tokenizer.rs                            (new: tiktoken o200k_base estimate)
    api_key_store.rs                        (new: ApiKeyStore trait + KeychainApiKeyStore + FakeApiKeyStore)
    count_tokens_client.rs                  (new: CountTokensClient trait + AnthropicCountTokensClient)
    compute.rs                              (new: count_text — orchestrates cache/tokenizer/client/calibration)
  adapters/
    claude_code/
      mod.rs                                (modify: `pub mod footprint_text;`, ClaudeCodeAdapter::compute_footprint, impl HarnessAdapter)
      footprint_text.rs                     (new: always_on_text, on_invoke_text, on_demand_files_text)
```

---

### Task 1: Add dependencies, verify the crate still builds

**Files:** Modify `src-tauri/Cargo.toml`

- [ ] Add to `[dependencies]`: `tiktoken-rs = "0.12"`, `sha2 = "0.11"`, `rusqlite = { version = "0.40", features = ["bundled"] }`, `reqwest = { version = "0.13", features = ["blocking", "json"] }`, `keyring = "4"`.
- [ ] Add to `[dev-dependencies]`: `mockito = "1"`.
- [ ] Run `cargo build --manifest-path src-tauri/Cargo.toml`; expect success.
- [ ] Commit: `chore: add footprint counter dependencies`

---

### Task 2: Domain footprint types

**Files:** Create `src-tauri/src/domain/footprint.rs`; modify `src-tauri/src/domain/mod.rs`

**Produces:**
```rust
pub enum TokenSource { Exact, Estimate }
pub enum TextConfidence { Native, Reconstructed }
pub struct LayerCount { pub tokens: u32, pub source: TokenSource }
pub struct AlwaysOnFootprint { pub count: LayerCount, pub confidence: TextConfidence }
pub struct Footprint {
    pub always_on: AlwaysOnFootprint,
    pub on_invoke: LayerCount,
    pub on_demand: LayerCount,
}
```
Pure data, no behavior beyond field access — matches the style of `domain::skill`. No `model_id` field anywhere on these types (ADR 0018: internal only, never on a type the UI would render).

- [ ] Write the types (`Debug, Clone, PartialEq` derives to match `domain::skill`'s style).
- [ ] `cargo test domain::footprint` — no tests needed beyond a compile check; add one trivial construction test for parity with the rest of the domain module's test style.
- [ ] Commit: `feat: add domain footprint types`

---

### Task 3: Content hashing

**Files:** Create `src-tauri/src/footprint/hashing.rs`; create `src-tauri/src/footprint/mod.rs`; modify `src-tauri/src/lib.rs`

**Produces:** `footprint::hashing::sha256_hex(content: &str) -> String`

- [ ] TDD: test that identical content hashes identically, different content hashes differently, and the output is a lowercase 64-char hex string (`sha2::Sha256::digest`, hex-encode manually or via a small helper — no extra hex crate needed, `format!("{:02x}", byte)` folded over the digest bytes).
- [ ] Wire `pub mod hashing;` into `footprint/mod.rs`; `mod footprint;` into `lib.rs`.
- [ ] Commit: `feat: add content hashing for token cache keys`

---

### Task 4: SQLite token cache + calibration table

**Files:** Create `src-tauri/src/footprint/cache.rs`; modify `src-tauri/src/footprint/mod.rs`

**Schema:**
```sql
CREATE TABLE IF NOT EXISTS token_cache (
    content_hash   TEXT PRIMARY KEY,
    tiktoken_count INTEGER NOT NULL,
    exact_tokens   INTEGER,
    exact_model_id TEXT,
    computed_at    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS calibration (
    model_id     TEXT PRIMARY KEY,
    factor       REAL NOT NULL,
    sample_count INTEGER NOT NULL,
    updated_at   TEXT NOT NULL
);
```

**Produces:**
- `cache::TokenCache::open(path: &Path) -> rusqlite::Result<Self>` (also accept `:memory:` for tests)
- `cache::TokenCache::get(&self, content_hash: &str) -> Option<CachedEntry>` — `CachedEntry { tiktoken_count: u32, exact: Option<(u32, String)> }`
- `cache::TokenCache::put_tiktoken(&self, content_hash: &str, tiktoken_count: u32)` — upsert, preserves any existing `exact_*` columns
- `cache::TokenCache::put_exact(&self, content_hash: &str, exact_tokens: u32, model_id: &str)` — upsert exact columns, then recomputes and upserts that `model_id`'s calibration row (`SUM(exact_tokens)/SUM(tiktoken_count)` over all rows where `exact_model_id = ?`)
- `cache::TokenCache::calibration_factor(&self, model_id: &str) -> Option<f64>`

- [ ] TDD, one test per behavior: fresh miss returns `None`; `put_tiktoken` then `get` round-trips; `put_exact` after `put_tiktoken` preserves the tiktoken count on the same row; `put_exact` on a stale `exact_model_id` (simulate a reference-model change) is overwritten by a new `put_exact` call with a new model id; `calibration_factor` is `None` before any exact sample and becomes `Some` after one, updating correctly after a second sample with a different ratio.
- [ ] Use `tempfile::tempdir()` + a real file path in tests (not `:memory:`) to also exercise `open`'s file-creation path at least once; `:memory:` is fine for the rest.
- [ ] Wire into `footprint/mod.rs`.
- [ ] Commit: `feat: add content-addressed SQLite token cache`

---

### Task 5: tiktoken estimator

**Files:** Create `src-tauri/src/footprint/tokenizer.rs`; modify `src-tauri/src/footprint/mod.rs`

**Produces:** `tokenizer::estimate_tokens(text: &str) -> u32` — `tiktoken_rs::o200k_base_singleton()` (or `o200k_base()` if the singleton needs no lock management simpler than a `OnceLock`; prefer the crate's own singleton helper since it already handles caching the loaded BPE tables), then `.encode_ordinary(text).len() as u32`.

- [ ] TDD: known short strings against a hand-verified token count is fragile across BPE table versions — instead test properties: empty string is 0 tokens; longer text has more tokens than a strict substring of it; identical input is deterministic (call twice, same result).
- [ ] Commit: `feat: add tiktoken o200k_base estimator`

---

### Task 6: API key store (keychain-backed, with a fake for tests)

**Files:** Create `src-tauri/src/footprint/api_key_store.rs`; modify `src-tauri/src/footprint/mod.rs`

**Produces:**
```rust
pub trait ApiKeyStore {
    fn get(&self) -> Option<String>;
    fn set(&self, key: &str) -> Result<(), ApiKeyStoreError>;
    fn delete(&self) -> Result<(), ApiKeyStoreError>;
}
pub struct KeychainApiKeyStore; // real impl, keyring::v1::Entry::new("skillmon", "anthropic-console-key")
```
`get()` swallows a "not found" `keyring::v1::Error` into `None` (no key configured is not an error state); other keychain errors also collapse to `None` since "can't read the key" and "no key" have the same consequence (fall back to estimate) — but log via `eprintln!` or a future logging facility so a real keychain failure isn't silent forever. `set`/`delete` surface real errors since those are user-initiated actions that should report failure in the UI.

- [ ] Write `ApiKeyStore` trait and `KeychainApiKeyStore` (real, `#[cfg(not(test))]`-free — it's fine to compile everywhere, just not exercised by the test suite against the real OS keychain).
- [ ] Write a `FakeApiKeyStore` (in `#[cfg(test)]`, backed by `RefCell<Option<String>>` or similar) used by every other module's tests that need an `ApiKeyStore`.
- [ ] One smoke test using `KeychainApiKeyStore` is acceptable only if gated so it doesn't run in a sandboxed/headless CI environment — default to **not** exercising the real keychain in the automated suite at all; rely on the trait boundary plus manual verification (this plan's Verification section) instead.
- [ ] Commit: `feat: add keychain-backed API key store`

---

### Task 7: count_tokens HTTP client

**Files:** Create `src-tauri/src/footprint/count_tokens_client.rs`; modify `src-tauri/src/footprint/mod.rs`

**Produces:**
```rust
pub trait CountTokensClient {
    fn count_tokens(&self, text: &str, model_id: &str, api_key: &str) -> Result<u32, CountTokensError>;
}
pub struct AnthropicCountTokensClient { base_url: String } // base_url overridable for mockito
```
POST to `{base_url}/v1/messages/count_tokens` with headers `x-api-key: {api_key}`, `anthropic-version: 2023-06-01`, JSON body `{"model": model_id, "messages": [{"role": "user", "content": text}]}` (per ADR 0006's citation of `docs.anthropic.com/en/api/messages-count-tokens`); parse `{"input_tokens": N}` from the response.

- [ ] TDD against `mockito`: successful response parses `input_tokens` correctly; non-2xx status (e.g. 401, 429) returns a typed `CountTokensError` variant, not a panic; malformed JSON body returns a typed error; network-unreachable (mockito server dropped / bad base_url) returns a typed error too. Four focused tests, one per failure shape, plus the happy path.
- [ ] Real client build uses `reqwest::blocking::Client::new()` with a short timeout (e.g. 10s) — an unbounded timeout would hang a background rescan indefinitely on a bad network.
- [ ] Commit: `feat: add count_tokens HTTP client`

---

### Task 8: Compute orchestration

**Files:** Create `src-tauri/src/footprint/compute.rs`; modify `src-tauri/src/footprint/mod.rs`

**Produces:** `compute::count_text(text: &str, cache: &TokenCache, api_key_store: &dyn ApiKeyStore, client: &dyn CountTokensClient, reference_model_id: &str) -> LayerCount`

Logic (mirrors ADR 0006 + 0018, decisions #1 and #6 above):
1. Hash `text`. Look up in cache.
2. If cache has `exact_tokens` and `exact_model_id == reference_model_id`, return `LayerCount { tokens: exact_tokens, source: Exact }`.
3. Compute (or reuse cached) `tiktoken_count`; `put_tiktoken` if not already stored for this hash.
4. If `api_key_store.get()` is `Some(key)`: call `client.count_tokens(text, reference_model_id, &key)`. On `Ok(n)`: `cache.put_exact(hash, n, reference_model_id)`, return `LayerCount { tokens: n, source: Exact }`. On `Err`: fall through to step 5.
5. Look up `cache.calibration_factor(reference_model_id)`. If `Some(factor)`, return `LayerCount { tokens: (tiktoken_count as f64 * factor).round() as u32, source: Estimate }`. If `None`, return `LayerCount { tokens: tiktoken_count, source: Estimate }` (uncalibrated).

- [ ] TDD with `FakeApiKeyStore` + a fake `CountTokensClient` (closures or a small test struct implementing the trait) + an in-memory `TokenCache`: no key configured → estimate, uncalibrated; no key, but a prior exact sample exists for another piece of text under the same model → calibrated estimate; key present, call succeeds → exact, and a second call for the *same text* hits the cache and doesn't call the client again (assert call-count on the fake client); key present, call fails → falls back to estimate exactly like the no-key case; a cached exact value under a stale `reference_model_id` is not trusted, triggers a fresh call.
- [ ] Commit: `feat: add footprint compute orchestration`

---

### Task 9: Always-on text sourcing (ADR 0016)

**Files:** Create `src-tauri/src/adapters/claude_code/footprint_text.rs`; modify `src-tauri/src/adapters/claude_code/mod.rs`

**Produces:**
```rust
pub struct AlwaysOnText { pub text: String, pub confidence: TextConfidence }
pub fn always_on_text(skill: &DiscoveredSkill, search_project_dirs: &[PathBuf]) -> AlwaysOnText
```
Algorithm: iterate `search_project_dirs` (caller passes all known repos' project dirs for personal/user-scoped-plugin skills, or just the owning repo's project dir for project/scoped-plugin skills — decision #7 above), most-recently-modified transcript first within each; for the first transcript containing an `.attachment.content` block with a `- {directory_name}: ` prefix, extract to the next `\n- ` or end-of-block, return `Native`. If no transcript anywhere in scope contains it, reconstruct `"- {directory_name}: {description}"` from `skill.frontmatter` and return `Reconstructed`.

- [ ] TDD: a fixture transcript with an attachment block containing several skill bullets — extracts the right one, respecting the `\n- ` boundary (doesn't bleed into the next skill's line); a skill absent from every transcript in scope falls back to `Reconstructed` with the frontmatter-built line; when the *same* skill appears in two transcripts in scope, the most-recently-modified one wins (mtime tiebreak, mirroring `transcript::find_active_repo`'s pattern already in the codebase).
- [ ] Reuse `discovery::transcript`'s file-listing helpers where possible rather than re-implementing directory walking.
- [ ] Commit: `feat: add always-on footprint text sourcing`

---

### Task 10: On-invoke and on-demand text sourcing (ADR 0017)

**Files:** Modify `src-tauri/src/adapters/claude_code/footprint_text.rs`

**Produces:**
- `on_invoke_text(skill: &DiscoveredSkill) -> String` — `format!("Base directory for this skill: {}\n\n{}", skill.dir_path.display(), skill.body)`
- `on_demand_file_texts(skill: &DiscoveredSkill) -> Vec<(PathBuf, String)>` — reads each `on_demand_files` entry's raw bytes as a lossy UTF-8 string (footprint counting needs text; a genuinely non-UTF8 bundled file is skipped with no fatal error, matching the discovery pipeline's fault-isolation posture)

- [ ] TDD: on-invoke template matches the exact prefix format from ADR 0017 (byte-for-byte, including the double newline); on-demand returns one entry per bundled file with its literal content; an unreadable/non-UTF8 file is skipped, not fatal.
- [ ] Commit: `feat: add on-invoke and on-demand footprint text sourcing`

---

### Task 11: HarnessAdapter trait + ClaudeCodeAdapter::compute_footprint

**Files:** Create `src-tauri/src/domain/harness.rs`; modify `src-tauri/src/domain/mod.rs`, `src-tauri/src/adapters/claude_code/mod.rs`

**Produces:**
```rust
// domain/harness.rs
pub trait HarnessAdapter {
    fn discover_skills(&self) -> DiscoveryResult;
    fn compute_footprint(&self, skill: &DiscoveredSkill) -> Footprint;
}
```
`ClaudeCodeAdapter` gains the dependencies `compute_footprint` needs (`TokenCache`, `Box<dyn ApiKeyStore>`, `Box<dyn CountTokensClient>`, reference model id constant) as fields, constructed via a richer `ClaudeCodeAdapter::new(claude_home, cache_path)` (real keychain + real HTTP client wired by default) plus a `#[cfg(test)]` constructor taking fakes directly for the trait's own tests. `compute_footprint` assembles: `always_on_text` (search scope resolved from the skill's `SkillId` variant per decision #7) → `compute::count_text` → `AlwaysOnFootprint`; `on_invoke_text` → `compute::count_text` → `LayerCount`; sum of `on_demand_file_texts` each through `compute::count_text` → single summed `LayerCount` (ceiling, per ADR 0017 — no per-file breakdown at this layer).

- [ ] TDD: a full fixture (skill dir + SKILL.md + one bundled reference file + a fake transcript containing the rendered bullet + a fake API key store with no key) exercises `compute_footprint` end-to-end and asserts on the resulting `Footprint`'s three layers, confidence, and token sources, using `FakeApiKeyStore`/fake `CountTokensClient`/an in-memory `TokenCache` throughout — no real network, no real keychain, no real Anthropic call in this suite.
- [ ] `impl HarnessAdapter for ClaudeCodeAdapter`.
- [ ] Run the full suite: `cargo test --manifest-path src-tauri/Cargo.toml`; expect every test from Tasks 1–11 green.
- [ ] Commit: `feat: wire footprint computation into ClaudeCodeAdapter via HarnessAdapter trait`

---

## Self-Review Notes

**Spec coverage** — checked against ADRs 0002, 0005 (n/a here), 0006, 0008, 0016, 0017, 0018, 0019, 0020, and `src-tauri/CONTEXT.md`'s new terms:
- Exact tier only via user-supplied Console key, never OAuth → Task 6 (`ApiKeyStore` never touches Claude Code's credential store — there's no code path that could).
- Cache keyed by content hash, `model_id` internal-only → Task 4's schema, Task 8's orchestration, no `model_id` on any `domain::footprint` type (Task 2).
- Always-on sourced from transcript, frontmatter fallback flagged → Task 9.
- On-invoke deterministic template, on-demand raw bytes → Task 10.
- Footprint recompute is hash-driven, no separate invalidation trigger → inherent to the content-addressed design (decision #1), nothing to implement beyond the cache itself.
- Reference model never user-facing → Task 2's types carry no model field; Task 11 threads `reference_model_id` as an internal constant, not a parameter sourced from user input anywhere.
- API key in OS keychain, never plaintext → Task 6, ADR 0020.

**Not in this plan** (explicitly out of scope, later work): the Tauri commands that expose `compute_footprint` results to the UI (and the `spawn_blocking` wrapping that boundary needs, decision #4); wiring `compute_footprint` into the ADR 0019 rescan/debounce loop; the settings UI for entering/deleting the API key; attributed usage (ADR 0005, separate metric entirely); any UI rendering of the `~` estimate labeling from DESIGN.md's UX decisions.

**Placeholder scan** — no TODOs, no stub methods; every task has a concrete, testable interface and algorithm description ready to implement as real code.

**Type consistency** — `TokenSource`, `TextConfidence`, `LayerCount`, `AlwaysOnFootprint`, `Footprint` defined once in Task 2, referenced identically by every later task; `TokenCache`, `ApiKeyStore`, `CountTokensClient` defined once each (Tasks 4, 6, 7) and consumed with matching signatures in Task 8 and Task 11.

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-02-footprint-counter.md`. Proceeding to implement inline in this session (per the original request to "write and implement"), task-by-task, verifying `cargo build`/`cargo test` after each.
