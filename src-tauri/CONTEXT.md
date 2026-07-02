# skillmon — Domain

The domain context: skills, plugins, the two metrics, transcripts, and the mutations skillmon performs.
This is the ubiquitous language for the Rust core (`src-tauri/`).
The UI context (`../src/CONTEXT.md`) is created lazily as panel-specific terms appear.
Keep this a glossary; implementation detail lives in `../docs/DESIGN.md` and `../docs/adr/`.

## Language

### Skills and where they live

**Skill**:
A `SKILL.md` file (YAML frontmatter plus Markdown body, optionally with bundled files) that a coding agent loads into context and invokes as a tool.
The unit skillmon lists, measures, and enables or disables.
_Avoid_: command, tool, prompt

**Skill identity**:
The stable key that survives rescans and lets footprint history, tombstones, quarantine-origin, and attributed usage all point at the same row: `Personal(name)`, `Project(repo_path, name)`, or `Plugin(marketplace, plugin, name)`.
`name` here is the **directory name**, not the frontmatter `name:` field — the two can diverge (observed in the wild: a personal skill directory named `connect-chrome` with frontmatter `name: open-gstack-browser`) and the directory is what's filesystem-stable and what mutations actually operate on.
Never keyed by plugin version — an upgrade must not orphan usage history or reset a tombstone.
Distinct from the footprint cache key, which is content-hash-based (see Context footprint).
_Avoid_: skill ID, UUID

**Declared name**:
The frontmatter `name:` field, shown to the user/model as the skill's label.
When it diverges from the directory name (part of skill identity), the UI surfaces both rather than silently picking one — the user may know the skill by either.
_Avoid_: display name

**Personal skill**:
A skill installed for the user at `~/.claude/skills/<name>/`, discovered one level deep, with no native enable/disable flag.
_Avoid_: user skill, global skill

**Project skill**:
A skill at `<repo>/.claude/skills/<name>/`, scoped to a single repository and only in context while you work in that repo.
_Avoid_: local skill, repo skill

**Active repo**:
The repo whose transcript was most recently written to. Determined by reading the real `cwd` from a record inside each `~/.claude/projects/*/` candidate's transcripts, never by decoding the encoded directory name, which is ambiguous on hyphenated paths (ADR 0014). Gates which project skills and which project/local-scoped plugins (see Live) count toward the global always-on total.
_Avoid_: current repo, cwd

**Plugin skill**:
A skill shipped inside a plugin, removable only by removing the whole plugin.
_Avoid_: bundled skill

**Plugin**:
A distributable bundle of skills (and optionally commands, agents, MCP servers) installed from a marketplace.
_Avoid_: extension, package, add-on

**Marketplace**:
The source registry a plugin was installed from.
Built-in marketplaces are never removed.
_Avoid_: registry, store, repo

**Plugin-locked**:
The state of a skill whose file lives under a plugin's install path, so it cannot be disabled or removed on its own — only its owning plugin can be removed.
_Avoid_: bundled, protected

**Install scope** (plugin):
Where a plugin's enablement was granted: `user` (global), `project` (the active repo's git-committed settings), or `local` (the active repo's gitignored settings). Provenance of the *enable*, not of the files — every scope shares the same on-disk cache.
_Avoid_: install location

**Live** (plugin):
A plugin is live if enabled in any applicable scope: always the global settings, plus the active repo's project and local settings when a repo is active. An OR across sources, not a precedence order. A plugin live nowhere contributes zero footprint even though it remains discovered.
_Avoid_: enabled (ambiguous — enabled in which scope?), active

### The two metrics

**Context footprint**:
The token size of a skill's content as it enters context — exact when the user has supplied their own Anthropic API key, a tight calibrated estimate otherwise. Either way a measurement of one fixed, known quantity, never an attribution guess.
skillmon's headline metric.
Never sourced from Claude Code's own OAuth credential: Anthropic's Consumer Terms of Service restrict that token to Claude Code and claude.ai.
_Avoid_: size, weight, cost

**Always-on layer**:
The rendered listing line(s) Claude Code injects for the skill on every request while it is enabled — the persistent tax that justifies disabling an unused skill.
Not fixed to "frontmatter `name` + `description`": it uses the directory name rather than the frontmatter `name:` field, and can carry extra rendered decorations beyond the description (observed: a "Voice triggers" line on a gstack-managed skill, absent on others).
Read from a live transcript when one exists (native, high confidence); reconstructed from raw frontmatter only for a skill no transcript has ever included, and flagged as lower confidence.
_Avoid_: frontmatter cost

**On-invoke layer**:
`"Base directory for this skill: {path}\n\n" + body`, loaded once when the skill is invoked — not just the raw body. Confirmed as a stable template across both invocation paths (Skill tool and slash command), so it is always computed directly; no transcript lookup needed, unlike always-on.
_Avoid_: skill body, body cost

**On-demand layer**:
Bundled reference files that load only if the body tells the agent to read them; reported as a ceiling, never in the headline. Measured as raw file bytes, not whatever tool-specific wrapper the agent happens to read them through (e.g. Read's line-numbered output) — which wrapper applies is a runtime choice, not a fact about the skill, so the ceiling deliberately doesn't chase it.

**Token source**:
Whether a layer's count is `Exact` (a live `count_tokens` call, cache-hit or fresh) or `Estimate` (calibrated `tiktoken`, used whenever no API key is present or a `count_tokens` call fails — the two collapse to the same fallback path, since either way exact wasn't available). Orthogonal to text confidence, which only the always-on layer carries.
_Avoid_: exactness, confidence (reserved for text confidence below)

**Text confidence**:
Only meaningful for the always-on layer: `Native` (the literal transcript-rendered line, ADR 0016) or `Reconstructed` (built from raw frontmatter because no transcript has ever included the skill yet). Independent of token source — a reconstructed line can still be counted exactly.
_Avoid_: accuracy

**Token cache**:
The content-addressed store keyed by `sha256(canonical content)` alone (ADR 0006). One row per hash holds the always-computed `tiktoken` count plus, once available, the exact `count_tokens` value and the reference model it was measured against. A skill's per-layer footprint is a lookup of that layer's current text hash, not a row keyed by skill or layer — identical content (e.g. two skills sharing a reference file) shares one row for free, and an edit's new hash is simply a fresh cache miss with no explicit invalidation step.
_Avoid_: footprint table, token store

**Calibration factor**:
`Σ exact_tokens / Σ tiktoken_tokens`, summed over every token-cache row that currently has an exact value for the reference model. Used to scale a raw `tiktoken` count into the estimate tier once at least one exact sample exists; absent any exact sample, the estimate tier is uncalibrated (raw `tiktoken`, no multiplier) rather than silently using a factor of 1 as if it meant something.
_Avoid_: correction factor, multiplier

**Reference model**:
The single, fixed model `count_tokens` is always called against — skillmon's internal choice, never a user-facing setting (ADR 0018). Kept on the token-cache row only so a future change to skillmon's own default invalidates stale exact values instead of mixing them with new ones.
_Avoid_: model setting, target model

**Console API key**:
A user-supplied Anthropic Console `ANTHROPIC_API_KEY`, entered explicitly into skillmon, that unlocks the exact tier. Stored in the OS keychain, never on disk in plaintext or otherwise (ADR 0020). Never Claude Code's own OAuth credential — that's off-limits on principle, not just unreliability (ADR 0006).
_Avoid_: token (ambiguous with footprint tokens), credential

**Attributed usage**:
Tokens spent in sessions while a skill was active.
A proxy for causation, not a bill — read as "tokens during this skill," never "tokens used by it."
_Avoid_: cost, spend, bill, tokens used

**Native attribution**:
The skill/plugin attribution the agent itself records on a turn.
Trusted verbatim where present.

**Reconstructed attribution**:
A fallback skill-stack walk over a transcript, used only where native attribution is absent, and flagged as lower confidence.
_Avoid_: guessed

**Work tokens**:
Input plus output tokens — the marginal, non-cached compute a skill's session caused.
The headline usage number.

**Cache-read tokens**:
Re-read context tokens, which dominate an order of magnitude or more; shown separately as "context tax," never in the headline.
_Avoid_: cached tokens

**Cache-write tokens**:
The footprint entering the cache; a secondary column.

### Sessions and history

**Transcript**:
An append-only session log the agent writes; the source for attributed usage.
_Avoid_: log, history file

**Sub-agent**:
A spawned agent run that writes its own transcript and whose cost is summed from that file.
_Avoid_: sidechain (internal term only)

**Tombstone**:
A retained "(removed)" history row for an uninstalled skill, so trend totals stay honest and reinstalling restores continuity.

### Actions

**Harness adapter**:
The boundary that abstracts one agent's specifics — where skills live, how footprint is read, how transcripts parse, how enable/disable is mutated.
v1 has a single Claude Code implementation.
_Avoid_: driver, backend

**Quarantine (disable)**:
skillmon's reversible way to disable a personal skill: move its directory out of the scan root, recording origin so it can be restored.
Distinct from uninstall.
_Avoid_: delete, remove

**Two-phase delete (trash)**:
Uninstall that moves a skill to a trash area first and purges only after confirmation, so no delete is irreversible until purge.
_Avoid_: delete
