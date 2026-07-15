# 5. Native-first attribution with reconstructed fallback

## Status

Accepted (primary algorithm and sub-agent default settled in grilling; hierarchical roll-up deferred as an implementation detail).
Native-first shipped in issue #5; the sub-agent include toggle shipped native-first in issue #13; the `parentUuid` reconstruction walk shipped version-gated in issue #12; the parent-spawn hierarchical roll-up shipped version-gated in issue #19 (see the Updates below).
Every deferred follow-up named by this ADR is now built.

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

## Update (issue #13: the sub-agent toggle ships native-first; a stale premise corrected; the parent-walk demoted)

Correction to the premise above and in the Context (`native attribution ... absent on sub-agent files`): it is **stale**.
Verified against all 1,455 real sub-agent files on this machine: sub-agent `assistant` records on current builds **do** carry their own native `attributionSkill`, and attribution absence spans the *same* Claude versions in the attributed and unattributed sets — so an unattributed sub-agent record means "no skill was active in that turn," not "a build too old to attribute."
Native own-attribution is therefore authoritative for crediting sub-agents too, exactly as it is on the main thread; the toggle is native-first, with no reconstruction.

This retires the blocker recorded in the #5 update.
The toggle's hard part was assumed to be the hierarchical roll-up (crediting a sub-agent file's tokens to the skill that spawned the parent `Task`).
Measured, that walk credits **0 files / 0 tokens** on the real corpus: native own-attribution already credits the full achievable set (248 files / 3.0M work tokens), and where both a child's own attribution and its parent-`Task` turn's attribution exist they agree 90/90 — the walk is redundant where it fires and would fabricate withheld attribution where it doesn't.
So the parent-`Task` roll-up is demoted to a **version-gated follow-up**: it would only ever help on a hypothetical build that attributes the parent turn but not the child record, and it must never run on builds where the child self-attributes.

What ships in issue #13 is native-first only: a backend `list_skills(include_subagents)` re-scan param that, when on, enumerates the sub-agent transcripts (`<session>/subagents/agent-*.jsonl`, including `subagents/workflows/wf_*/`) as a second pass and folds their own deduped `message.usage` into the totals, tagged `is_subagent = 1` by provenance so a mislabeled `isSidechain` cannot leak sub-agent tokens into the default headline.
Sub-agent refs feed only the usage pass, never the listing index — a sub-agent file's own `skill_listing` attachment must not pollute always-on.
Excluded-by-default is unchanged.

## Update (issue #19: the parent-spawn roll-up ships version-gated; its gate window is pre-attribution, so the parent resolves native-first too)

The last deferred follow-up, the parent-`Task` hierarchical roll-up, is built.
It credits a sub-agent file's own work to the skill that spawned it, linking `agent-<id>.jsonl` to its spawn through the sibling `agent-<id>.meta.json`'s `toolUseId`, matching that id against a `tool_use` block in the session's own `<project_dir>/<sessionId>.jsonl`, and reading that turn's attribution.
The spawn tool is `Agent` on current builds and `Task` on older ones; both are matched, and a matching id on any other tool is ignored rather than trusted.

**Correction to issue #19 as filed.**
It specified two things that cannot both hold: gate the walk below the attribution-introduction build (2.1.146, issue #12's constant), *and* read the spawning parent record's native `attributionSkill`.
But 2.1.146 is *defined* as the build where native attribution begins.
So inside the gate window the parent turn has no native `attributionSkill` either, by the constant's own meaning.
A native-only read would therefore resolve nothing on every build the gate admits, and the feature would be shipped dead code.
Confirmed by mutation: with the fallback removed, every test asserting a real credit fails.

So the parent turn's attribution resolves **native-first, with issue #12's skill-stack walk as the fallback**, which is this ADR's own algorithm applied one level up.
The walk reuses #12's rules exactly, including credit-before-push: a spawn is credited to the skill already active, never to one the same record starts.
A spawn with no skill either way is dropped, never guessed.
Chaining reconstruction onto reconstruction is honest because the label is honest.
Rolled-up rows are tagged `reconstructed`, which is already sticky across a fold (ADR 0003), so a rolled-up credit can only ever downgrade a skill's confidence badge, never inflate it.

**The alternative reading is empirically dead.**
One could instead gate on a band: builds that attribute the parent turn but not the child record.
No such band exists.
Measured across this machine's corpus, sub-agent attribution presence and absence span the *same* builds (2.1.172: 31 of 115 files attributed; 2.1.181: 0 of 306; 2.1.201: 125 of 167), which is the #13 finding restated.
An unattributed sub-agent record means "no skill was active," not "a build too old to attribute."
There is no version below which the child stops self-attributing and above which it starts.

**Yield remains zero on real data, and that is the design working.**
Verified through the full pass against a real `~/.claude` (the `#[ignore]`d `real_claude_home_subagent_rollup`): of 1,239 sub-agent files, 170 carry a `toolUseId` linkage at all (the rest are workflow files, which have none) and **0** are below the gate, the oldest build on disk being 2.1.170.
The roll-up therefore contributes 0 reconstructed credits, and every total stays byte-identical to #13's native-first figures.
That negative assertion is the regression guard: a change that widened the gate would light it up.

Three details are load-bearing, two of them deliberately asymmetric with #12's walk.

- **No `!is_tail` guard.**
  #12's reconstruction is whole-file-stateful (it carries the active skill across lines), so a byte-offset tail that starts mid-file would miscredit and is refused.
  The roll-up is stateless per record, since every unattributed record credits the same spawning skill, so a tail credits correctly and is allowed.
- **The gate is checked twice, for two different reasons.**
  `rollup_subagent_rows` gates itself, as `reconstruct_usage_rows` does, because the safety invariant belongs to the pure function rather than to its caller.
  `rollup_rows_for` gates again first purely as a fast path, so the common case (every file on a real machine) bails after one cheap version probe instead of reading a meta sidecar and re-reading a parent transcript.
  Removing the fast-path gate changes no outcome, only I/O; removing the pure function's gate is what a current-build fabrication would slip through, so it is pinned by its own test.
- **An unreadable parent is never cached as "no spawns".**
  Error is not absence, the same distinction `enumerated_dirs` draws for the prune (ADR 0024).
  Caching a failed read would strip the credit from every sibling sub-agent of that session for the whole scan; not caching it costs at most one retry per sibling.

The `tool_use_attribution(toolUseId -> skill, plugin)` table issue #19 floated is built per scan and memoized per parent transcript, not persisted, taking the issue's own "(or a per-scan parent re-read)" alternative: the many sub-agent files of one session walk their shared parent once, and no schema surface is added for a walk that fires on no current build.
Native own-attribution stays authoritative throughout.
Only records lacking their own `attributionSkill` are roll-up candidates, so the roll-up can neither displace a native credit nor double-count a message the native pass already counted, and its rows are `INSERT OR IGNORE` regardless.
Sums come from the sub-agent file's own `message.usage`, never a parent's `toolUseResult.totalTokens`, and rows are stamped `is_subagent` by provenance so the include toggle gates them.
