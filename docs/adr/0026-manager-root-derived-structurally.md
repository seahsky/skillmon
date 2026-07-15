# 26. Derive a skill's manager structurally, and don't model upstream origin

## Status

Proposed.

## Context

Users ask "where did this skill come from?" — expecting buckets like *Claude official*, *from superpowers*, *my own*.
Measured against a real `~/.claude` (71 personal skills), that mental model does not survive:

- **"From superpowers" is already answered.** Superpowers is a plugin, and `SkillReport` already carries `plugin` + `marketplace`.
- **"Claude official" means three incompatible things**: skills compiled into the CLI binary (`dataviz`, `security-review` — no files on disk, so skillmon can never see them); plugins in an Anthropic-*owned* marketplace (`claude-plugins-official`, which hosts `superpowers`, authored by Jesse Vincent, and `serena`, tagged `community-managed`); and plugins Anthropic actually *authored* (`frontend-design`). Superpowers satisfies the second but not the third, so "official" and "superpowers" are not opposing buckets.
- **"My own" was empty.** All 71 personal skills are third-party: 46 gstack, 20 mattpocock via `~/.agents`, 5 hand-installed.

What users actually need from that column is not authorship but **who will restore this if I remove it** — which is also the precondition for a safe delete button.
That is knowable from the filesystem alone.

## Decision

Model a skill's provenance as a **manager root**: the parent of the *resolved* skill directory, reached by following the skill directory's own symlink or, when the directory is real, its `SKILL.md`'s symlink.
Derive it structurally. Do not read any managing tool's manifest to establish it.

Expose two independent facts, never collapsed into one:

- **manager root** — does something else own my content? (`manager_root: Option<PathBuf>`)
- **dependent skills** — do other discovered skills resolve into me?

Deliberately **do not** model upstream origin (the repo/author a skill came from).

## Considered Options

- **Structural + git remote.** A walk up to the nearest `.git` yields real provenance for gstack (`github.com/garrytan/gstack`). Rejected: it answers the origin question, which is not the one that gates removal, and it is silent for `~/.agents` (not a git repo) and the 5 hand-installed dirs.
- **Structural + `.skill-lock.json`.** Adds `mattpocock/skills` for the 20 `.agents` skills. Rejected for *detection*: it hardcodes a bespoke, versioned, third-party format owned by a tool serving 14 agents, to populate a column that does not need it. (Removal is a different matter — see ADR 0027.)
- **A trust tier** (Anthropic / third-party / mine). Rejected: 69 of 71 rows land in "third-party". The column earns nothing.

## Consequences

Two independent fields, rather than one `Source` enum, because the two facts are orthogonal and conflating them is actively dangerous: `~/.claude/skills/gstack` is unmanaged **and** has 46 dependents, so "unmanaged" alone would read as "safe to delete" on the single most destructive row on disk (1.1 GB, 14,203 files).

The dependent count is deferred to issue #30 with the column it feeds; issue #25 landed `manager_root` alone, since that is what removal safety turns on and what the detection fix already computes.

Dependents are counted by an **ancestor** test, not path equality. Equality happens to work for gstack's 46 shims today but would silently return zero if the tool ever nested its skills one level deeper — and "what breaks if I remove this directory" means anything resolving into it at any depth.

An unfamiliar tool is described correctly with no new code: its skills report a manager root and are known to be managed. Only the label is generic.

The column shows manager-root **paths** (`~/.claude/skills/gstack`, `~/.agents/skills`), not invented product names. No basename rule works: `gstack` is right but `skills` (from `~/.agents/skills`) is useless, and any fix is a heuristic pile.

**This requires fixing detection, which is currently wrong.** `src-tauri/src/adapters/claude_code/discovery/scan.rs:31` lstats only the skill *directory*, so all 46 gstack shims — a real dir whose `SKILL.md` is a symlink — record `is_symlink: false`. Present detection catches 20 of 66 managed skills. Both link positions must be probed.

Detection lives inside `adapters/claude_code/`, which is correct per ADR 0002 and must stay that way: *where* an entry points is a fact about Claude Code's skills directory. What the thing it points at can do about removal is not (ADR 0027).

`SkillReport` must widen: it carries no path, no symlink data, and no stable id today.
