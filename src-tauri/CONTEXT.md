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

**Personal skill**:
A skill installed for the user at `~/.claude/skills/<name>/`, discovered one level deep, with no native enable/disable flag.
_Avoid_: user skill, global skill

**Project skill**:
A skill at `<repo>/.claude/skills/<name>/`, scoped to a single repository and only in context while you work in that repo.
_Avoid_: local skill, repo skill

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

### The two metrics

**Context footprint**:
The deterministic, exact token size of a skill's content as it enters context.
skillmon's headline metric.
_Avoid_: size, weight, cost

**Always-on layer**:
The frontmatter (`name` + `description`) that enters context on every request while the skill is enabled — the persistent tax that justifies disabling an unused skill.

**On-invoke layer**:
The SKILL.md body, loaded once when the skill is invoked.

**On-demand layer**:
Bundled reference files that load only if the body tells the agent to read them; reported as a ceiling, never in the headline.

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
