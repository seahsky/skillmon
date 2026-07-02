# 3. Track both context footprint and attributed session usage

## Status

Accepted.

## Context

There are two honest things to measure about a skill, and they answer different questions.
Context footprint (the token size of the skill's content) is deterministic and exact.
Attributed session usage (tokens spent while the skill was active) is an estimate dominated by whatever task the user was doing, and cache-read inflates it 10–100×.
Shipping only one hides real information; blending them into a single number lies.

## Decision

Track both, with a strict hierarchy: deterministic **context footprint is the headline**; fuzzy **attributed session usage is secondary and visibly demoted**.

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
