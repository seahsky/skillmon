# 14. Discover project repos and the active repo via transcript `cwd`, not directory-name decoding

## Context

Project-skill discovery needs a `repo_path` (part of skill identity, see `../../src-tauri/CONTEXT.md`) and a way to find "the active repo" for the always-on total (DESIGN.md), plus the full set of repos for the per-repo inventory sections (UX decision 5).
`~/.claude/projects/<encoded-cwd>/` looks like an index of every repo Claude Code has run in, but the encoding replaces every `/` with `-`, which is lossy: a real path segment containing a hyphen (observed on disk, e.g. `bas-ai-tools-k1-sandbox-tests`) makes decoding ambiguous — there is no way to tell where the path separators were.
Inspecting a live transcript showed each JSONL record already carries a plain `"cwd"` field with the real, unambiguous path.

## Decision

Treat `~/.claude/projects/*/` as a candidate list only, never decode the directory name.
For each candidate, read one `cwd`-bearing record from any transcript inside it to get the real `repo_path`.
"Active repo" is whichever project directory's transcript was most recently written to.

## Consequences

- A repo with a hand-created `.claude/skills/` directory that Claude Code has never been run in is invisible to skillmon until the user runs `claude` there at least once. Accepted: matches the domain definition of a project skill as "only in context while you work in that repo."
- Project-repo discovery piggybacks on the same transcript files attributed-usage parsing already reads (ADR 0005), so no separate filesystem walk or permission surface is needed.

## Options considered

- **Decode the encoded directory name** — ambiguous on hyphenated paths; rejected.
- **Unbounded filesystem walk from a configured root** — finds repos Claude Code has never touched, but is expensive, needs a root the user must configure, and conflicts with the depth-1/no-speculative-scanning posture used elsewhere (ADR 0002); rejected for v1.
- **Peek at real `cwd` inside each transcript directory** — chosen.
