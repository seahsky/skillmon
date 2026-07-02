# 11. Repository license — MIT OR Apache-2.0

Status: Accepted.

## Context

The svelte-ts template hardcodes `"license": "MIT"`.
Rust/Tauri projects conventionally dual-license MIT OR Apache-2.0 (permissive, patent grant from Apache-2.0, ecosystem norm).

## Decision

Dual-license **MIT OR Apache-2.0**.
`LICENSE-MIT` + `LICENSE-APACHE` at the root (canonical text from GitHub's license API); SPDX `MIT OR Apache-2.0` set in both `package.json` and `src-tauri/Cargo.toml [package].license`.

## Consequences

- Keep the two license fields in sync with the files.
- Contributions are inbound under the same dual terms.
