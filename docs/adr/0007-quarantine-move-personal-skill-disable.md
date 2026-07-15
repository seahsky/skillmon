# 7. Disable personal skills by quarantine move

## Status

Proposed, and amended by ADR 0027 before any of it was implemented.
ADR 0027 collapses this ADR's two mechanisms (quarantine, trash) into one reversible move-the-entry-out operation carrying a retention intent, scopes removal to the *entry* rather than the content it resolves to, and records a hazard this ADR does not: a managing tool silently reverting a quarantine.
Read 0027 first; the parts of this ADR it does not touch (plugins via the `claude plugin` CLI, cross-device `rename(2)` fallback, `.in_use/<pid>`) still stand.

## Context

Claude Code has no native per-personal-skill enable/disable flag (confirmed: `claude plugin details` and `@skills-dir` both return not-found).
Personal-skill discovery is depth-1 only, so moving a skill dir out of `~/.claude/skills/` reliably un-discovers it.
Many personal entries are symlinks managed by other tools (gstack, `.agents`), which may recreate them.
Plugin operations have a format-stable CLI (`claude plugin …`); personal skills do not, so skillmon must supply its own reversible mechanism.

## Decision

Disable a personal skill by `rename(2)` of `~/.claude/skills/<X>/` to `~/.claude/skillmon/disabled/skills/<X>/`, recording the original path and any symlink target in `~/.claude/skillmon/state.json`; enable is the reverse.
Uninstall is two-phase: move to `~/.claude/skillmon/trash/<ts>/` then purge after confirmation.
Project-skill quarantine stays inside that repo's `.claude/skillmon/…` to preserve project locality.
For plugins, prefer the `claude plugin {disable,enable,uninstall}` CLI over hand-editing `installed_plugins.json`; snapshot the affected files to `skillmon/backups/<ts>/` before any direct JSON edit; respect `.in_use/<pid>` before deleting any cache dir.

## Consequences

- Every mutation applies to new sessions only; the UI shows a "restart Claude Code to apply" nudge.
- Disabling a symlinked entry only moves the pointer; the managing tool may recreate it, so skillmon detects symlinks, records the target, and warns that true removal needs action at the source.
- `rename(2)` is atomic only within one filesystem; if source and dest differ by device, fall back to copy+fsync+swap.

## Options considered

- **In-place rename `SKILL.md` to `SKILL.md.disabled`** (a dir with no `SKILL.md` is not a skill) — simpler and keeps the dir in place, but mutates inside a possibly tool-managed dir and is easy to miss; kept as the alternate, not primary.
- **Delete outright** — not reversible; rejected in favor of two-phase trash.
- **Quarantine move out of the scan root** — chosen: reversible, keeps origin state, clearly un-discovers the skill.
