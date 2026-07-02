# 6. Footprint tokenizer — exact count_tokens, cached by content hash, tiktoken fallback

## Status

Proposed.

## Context

There is no public exact offline tokenizer for current Claude models.
`tiktoken` is OpenAI BPE and undercounts Claude tokens ~15–20% on text and more on code, biased low.
The tokenizer is model-generation-specific (Opus 4.7 produces ~30% more tokens than its predecessor), so any number is meaningless without naming a reference model.
The only exact method is the `count_tokens` REST endpoint, and there is no official Anthropic Rust SDK.
A skill's footprint changes only when it is installed, updated, or edited.

## Decision

Make the headline the exact `count_tokens` value, cached by `(sha256(canonical content), model_id)`.
Call the REST endpoint directly from the Rust core with `reqwest` (`x-api-key`, `anthropic-version: 2023-06-01`) on a cache miss only; steady state is fully offline.
For the transient "changed but not yet re-counted" state, fall back to `tiktoken-rs` `o200k_base` multiplied by a locally computed calibration factor (`Σ api_tokens / Σ tiktoken_tokens` over the user's own skills), flagged `token_source = heuristic_estimate` and never silently occupying the headline slot.
Headline = always-on (frontmatter) + on-invoke (body); bundled files are a separate on-demand ceiling.

## Consequences

- Every stored footprint records its `model_id`; the UI names the reference model.
- Auth is the open fork: reuse `ANTHROPIC_API_KEY` if present, try the `~/.claude` OAuth token with `anthropic-beta: oauth-2025-04-20` (scope not guaranteed), else prompt for a key.
- First run and any edit touch the network once per changed file; the honesty caveat that this is exact file size, not a live per-request bill, is surfaced in the UI.

## Options considered

- **Ship the legacy Claude-2 offline BPE** — invalid for Claude 3/4+; rejected.
- **tiktoken as the headline** — directionally wrong (undercounts), would make every skill look cheap; rejected as headline, kept as flagged fallback.
- **Always call the API live** — needs network on every view; rejected in favor of content-hash caching.
- **Exact count_tokens cached by hash+model, tiktoken calibrated fallback** — chosen.
