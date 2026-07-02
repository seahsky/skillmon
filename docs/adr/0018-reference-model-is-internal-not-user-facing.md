# 18. Reference model is a fixed internal default, never surfaced as a user choice

## Context

ADR 0006 requires naming a reference model for exact counts, since generational gaps run ~30% and an unlabeled number is meaningless.
Grilling this, the instinct was to make the count "model agnostic" by averaging across models — but that reproduces the exact flaw ADR-0006 already rejected for tiktoken-as-headline: a number that is precisely true for no real model.
The underlying want wasn't a blended number, it was not having to think about which model at all.

The two tiers from ADR 0006 split cleanly here. Without an API key (most users), `tiktoken` doesn't measure any specific Claude model in the first place — it is already inherently model-agnostic, so attaching a model label to an estimate would falsely imply precision about a model it never touched. With a key, `count_tokens` genuinely needs a model parameter and produces a real, exact number for whichever one is named.

## Decision

Drop `model_id` as a user-facing concept entirely.
The estimate tier (no API key) shows no model label — it was never measuring one.
The exact tier (API key present) always calls one fixed model that skillmon itself chooses internally (current-gen Sonnet at time of writing); the user never selects or sees a model setting.
`model_id` is retained only as an internal cache-key field (`(sha256(canonical content), model_id)`, per ADR 0006) so that if skillmon's own internal default ever changes, cached counts against the old default are recognized as stale and recomputed — not silently mixed with the new ones.

## Consequences

- No settings surface for "which model" — one less decision for the user, and one less way to end up staring at an unlabeled or misleading number.
- Updating skillmon's internal default model (e.g. when a new generation ships) invalidates the exact-tier cache wholesale; this is an app-update event, not a runtime toggle, and should recount in the background rather than blocking the UI.
- If a future need arises to show the reference model (e.g. a user wants to know "is this counting for Opus or Sonnet"), it can be surfaced as a read-only fact in an "about this number" tooltip — still not a choice, just a disclosure.

## Options considered

- **Average across a basket of models** — produces a number true for none of them, reproducing ADR-0006's rejected blended-headline flaw; rejected.
- **Let the user pick a model in settings** — solves nothing over a good fixed default, and reintroduces the exact per-model complexity the user was trying to avoid; rejected.
- **One fixed internal default, no user-facing model concept, model_id kept only as an internal cache-invalidation key** — chosen.
