# 5. Native-first attribution with reconstructed fallback

## Status

Accepted (primary algorithm and sub-agent default settled in grilling; hierarchical roll-up deferred as an implementation detail).
Native-first shipped in issue #5; reconstruction and the sub-agent include toggle are named follow-ups (see the Update below).

## Context

The brief assumed a heuristic span ("next Skill ends the previous").
Investigation found Claude Code already writes native attribution: main-thread `assistant` records carry `attributionSkill` and `attributionPlugin` (null for personal skills), with genuine stack semantics and real `null` gaps between skills.
Native attribution is present on recent builds (2.1.159–2.1.195) but absent on sub-agent files and pre-attribution builds, so a fallback is still required.
Cache-read tokens dominate 10–100×, and resume/branch/compact duplicate `message.id` up to 11×.

## Decision

Resolve each `assistant` record's attributed skill in priority order: (1) trust native `attributionSkill`/`attributionPlugin` verbatim where present; (2) otherwise walk the file's `parentUuid` chain maintaining a skill stack (push on `Skill` tool_use, credit each turn to stack top, pop on the next same-depth push or a fresh human turn), tagged `attribution_source = reconstructed`.
Sum three token buckets per `(skill, plugin)`: work tokens (`input + output`, headline), cache-write, and cache-read (shown separately, never in the headline).
Dedup by `message.id` with `INSERT OR IGNORE`; parse incrementally via byte-offset checkpoints; sum sub-agent cost from the sub-agent file itself (never `toolUseResult.totalTokens`).

## Consequences

- Recent sessions get high-confidence native attribution; older/sub-agent turns degrade gracefully to reconstructed spans, always flagged so the UI can show a confidence badge.
- Sub-agent tokens are **excluded** from the headline usage number by default (they can over-credit a skill that merely spawned agents), with a user toggle to include them. Decided in grilling.
- Remaining implementation detail (not user-facing): whether to expose hierarchical roll-up (credit inner tokens to ancestors) vs top-of-stack only. Deferred to build time.

## Options considered

- **Pure heuristic windowing (original brief)** — less reliable than the field Claude Code already computes; rejected as the primary path, retained only as the reconstruction fallback.
- **Native-only** — leaves sub-agents and old builds unattributed; rejected.
- **Native-first + reconstructed fallback + message.id dedup** — chosen.

## Update (issue #5: native-first shipped; the attribution field shape corrected; two follow-ups split off)

Correction to the Context above: "`attributionSkill` (null for personal skills)" is imprecise and, read literally, would zero every personal skill.
Verified against real transcripts: it is **`attributionPlugin`** that is null for personal skills; **`attributionSkill` carries the bare directory name** for personal/project skills (e.g. `ship`, `grilling`) and the `plugin:name` form for plugin skills (e.g. `superpowers:executing-plans`, with `attributionPlugin` = `superpowers`).
`message.id` (the dedup key) lives at `message.id`, distinct from the record-level `uuid`; usage is at `message.usage.{input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens}`.
The join is therefore by a `(plugin, name)` key, never the directory name alone: two plugins can share a skill name (`impeccable:frontend-design` vs `frontend-design:frontend-design`), and marketplace is absent from attribution.

What shipped in issue #5 is native-first only, over main-thread transcripts, deduped by `message.id`, incremental (ADR 0024).
Two pieces are deliberately deferred as named follow-ups, mirroring the #2/#3 splits:

- **`parentUuid` skill-stack reconstruction** is deferred. Measured on a real `~/.claude`: native attribution is present on 144 of 145 files, and attribution absence spans *current* Claude versions (the same builds appear in both the attributed and unattributed sets), so absence means "no skill was active," not "pre-attribution build." Walking a stack on absence would fabricate attribution Claude deliberately withheld. The reconstruction walk is a version-gated follow-up; the `attribution_source` field (`native` | `reconstructed`) reserves the seam.
- **The sub-agent include toggle** is deferred to a follow-up (#5b). Exclude-by-default (the shipped half) is free: the enumeration never descends into `subagents/`. The toggle's hard part, crediting a sub-agent file's tokens to the skill that spawned it, is the hierarchical roll-up this ADR already defers, so it is blocked on that. When built it must be a backend `list_skills(include_subagents)` re-scan param writing `is_subagent = 1` rows into the same store, never a frontend filter.
