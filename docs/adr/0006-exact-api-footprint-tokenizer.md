# 6. Footprint tokenizer — exact count_tokens with a user-supplied API key, tiktoken as the honest default

## Status

Accepted.

## Context

There is no public exact offline tokenizer for current Claude models.
`tiktoken` is OpenAI BPE and undercounts Claude tokens ~15–20% on text and more on code, biased low.
The tokenizer is model-generation-specific (Opus 4.7 produces ~30% more tokens than its predecessor), so any number is meaningless without naming a reference model.
The only exact method is the `count_tokens` REST endpoint, and there is no official Anthropic Rust SDK.
A skill's footprint changes only when it is installed, updated, or edited; the text actually measured for the always-on layer is the rendered transcript line, not raw frontmatter (ADR 0016).

Auth was researched, not assumed. `count_tokens` only documents `x-api-key` auth (`docs.anthropic.com/en/api/messages-count-tokens`); Claude Code's own OAuth token — both the interactive session token and the long-lived token from `claude setup-token` — is documented as "scoped to inference only" (`code.claude.com/docs/en/authentication`) and is rejected by endpoints expecting `x-api-key`.
More decisively: Anthropic's Consumer Terms of Service (per a Feb-2026 policy clarification) restrict OAuth tokens to Claude Code and claude.ai specifically — using one from a third-party tool is a ToS violation, not merely an unreliable path.
That rules out every OAuth-based route on principle, not just on reliability, so skillmon must never attempt to read or reuse Claude Code's OAuth credential for this purpose.

This means most skillmon users — Claude Pro/Max subscribers with no separate API-billing key — cannot get exact counts by default.
The estimate is not a rare transient fallback; it is the real default experience for most installs.

## Decision

If the user has supplied a Console `ANTHROPIC_API_KEY` (their own, entered explicitly in skillmon — never read from Claude Code's credential store), the headline is the exact `count_tokens` value, cached by `(sha256(canonical content), model_id)`.
Call the REST endpoint directly from the Rust core with `reqwest` (`x-api-key`, `anthropic-version: 2023-06-01`) on a cache miss only; steady state is fully offline.
Without a key, every footprint is `tiktoken-rs` `o200k_base` multiplied by a locally computed calibration factor (`Σ api_tokens / Σ tiktoken_tokens` over the user's own skills, once any are exact — otherwise uncalibrated), flagged `token_source = heuristic_estimate` and never silently occupying the headline slot as if it were exact.
Headline = always-on (per ADR 0016) + on-invoke (body); bundled files are a separate on-demand ceiling.

## Consequences

- Every stored footprint records its `token_source` (exact/estimate), visibly distinguished in the UI. `model_id` is retained too, but purely as an internal cache-key detail, never surfaced as a user-facing model choice or label (ADR 0018).
- No OAuth path is attempted, ever, on any tier — not as a fallback, not opportunistically. The only way to unlock exact counts is the user pasting a Console API key into skillmon; onboarding says so plainly ("add an API key for exact counts; without one you'll see a calibrated estimate").
- The estimate is most users' permanent, not transient, experience. skillmon's UI and DESIGN.md framing must own this honestly rather than imply "exact" is the common case.
- First run and any edit touch the network once per changed file, only when a key is present; the honesty caveat that this is exact file size, not a live per-request bill, is surfaced in the UI.

## Options considered

- **Ship the legacy Claude-2 offline BPE** — invalid for Claude 3/4+; rejected.
- **tiktoken as the headline** — directionally wrong (undercounts), would make every skill look cheap; rejected as headline, kept as the flagged default.
- **Always call the API live** — needs network on every view; rejected in favor of content-hash caching.
- **Reuse Claude Code's OAuth token (session or `setup-token`)** — researched in detail; rejected on principle, not just reliability: Anthropic's Consumer ToS restricts OAuth tokens to Claude Code and claude.ai, so a third-party tool using one is a policy violation regardless of whether the request would technically succeed.
- **Exact count_tokens cached by hash+model when a user-supplied API key exists, tiktoken calibrated estimate as the real default otherwise** — chosen.
