# 15. Plugin enablement merges three settings files, gated by the active repo

## Context

The original design (DESIGN.md, "Plugin state is registry-driven") assumed a single global toggle: `~/.claude/settings.json → enabledPlugins`.
Researching Claude Code's plugin-scope feature (official docs, `code.claude.com/docs/en/discover-plugins.md` and `settings.md`) while grilling plugin discovery (ADR 0014's neighbor decision) found that plugins can be installed at `user`, `project`, or `local` scope.
The `scope` field recorded in `~/.claude/plugins/installed_plugins.json` is install provenance only — every plugin's files land in the same shared `~/.claude/plugins/cache/`, there is no repo-local cache.
What actually gates enablement can live in three places: `~/.claude/settings.json` (global), `<repo>/.claude/settings.json` (project, git-committed, team-shared), and `<repo>/.claude/settings.local.json` (local, gitignored, personal).

## Decision

A plugin is live — its skills contribute to footprint and can be attributed to usage — if `enabledPlugins["<plugin>@<marketplace>"]` is `true` in **any** of: the global settings file, or the active repo's `settings.json` or `settings.local.json`.
Project- and local-scoped entries are gated by the same "active repo" concept used for project skills (ADR 0014): they only count while that repo is the active one.
This is an OR merge, not a precedence order — any one enabling source makes the plugin live.

## Consequences

- Plugin liveness becomes contextual (depends on the active repo) in the same way project-skill footprint already is, not a single fixed fact per plugin.
- A plugin enabled only at project scope in a repo you are not currently in is discovered (it's still in `installed_plugins.json`) but shown as inactive/zero footprint, mirroring how other repos' project skills are shown but not summed.
- DESIGN.md's "Plugin state is registry-driven" section needs correcting — it currently describes only the global toggle.

## Options considered

- **Global toggle only (original design)** — factually wrong; a project- or local-scoped enable would be invisible to skillmon. Rejected.
- **Precedence order (e.g. local overrides project overrides global)** — Claude Code's own semantics for this weren't confirmed either way; OR-merge is the simpler, safer default until contradicted by evidence. Chosen provisionally.
- **OR merge across all three, active-repo-gated for project/local** — chosen.
