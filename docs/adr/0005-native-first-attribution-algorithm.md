# 5. Native-first attribution with reconstructed fallback

## Status

Accepted (primary algorithm and sub-agent default settled in grilling; hierarchical roll-up deferred as an implementation detail).

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
