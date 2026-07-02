# 3. Track both context footprint and attributed session usage

## Status

Accepted (footprint's "always exact" framing narrowed by ADR 0006: exact requires a user-supplied API key, a calibrated estimate otherwise — see Consequences).

## Context

There are two honest things to measure about a skill, and they answer different questions.
Context footprint (the token size of the skill's content) is a measurement of a known, fixed piece of text — exact when possible, and even when it isn't (no API key, see ADR 0006), it is a tight calibrated estimate of one unambiguous quantity.
Attributed session usage (tokens spent while the skill was active) is a different, harder kind of estimate: not "how many tokens is this text," but "which of the session's tokens should this skill get credit for" — a causation question, dominated by whatever task the user was doing, with cache-read inflating it 10–100×.
Shipping only one hides real information; blending them into a single number lies.

## Decision

Track both, with a strict hierarchy: **context footprint is the headline** (exact or a tight calibrated estimate, per ADR 0006 — either way a measurement, not an attribution guess); **attributed session usage is secondary and visibly demoted** (always fuzzy, by kind, not by missing a key).

The footprint headline is always-on + on-invoke tokens.
The usage number is work tokens (`input + output`), with cache-read shown separately and never folded in, and it is labeled "tokens during this skill," not "used by it."

## Consequences

- The default sort key and toast thresholds must each pick a metric explicitly (see open questions); they are not interchangeable.
- The UI needs a confidence/estimate treatment for the usage number distinct from the exact footprint.
- Two pipelines exist: a tokenizer/cache path (footprint) and a transcript-parsing path (usage), joined on a stable `skill_key`.

## Options considered

- **Footprint only** — trustworthy but ignores the "am I burning tokens" question; rejected.
- **Usage only** — the number users expect, but it is an estimate and would carry no reliable baseline; rejected.
- **Both, footprint headline + usage secondary** — chosen.
