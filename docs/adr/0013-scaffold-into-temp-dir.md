# 13. Scaffold Tauri into a temp dir, never in place

Status: Accepted.

## Context

The repo already held CLAUDE.md, CONTEXT-MAP.md, docs/, and src-tauri/CONTEXT.md before scaffolding.
create-tauri-app on a non-empty directory (with `--force` or an interactive confirm) recursively deletes everything except a top-level `.git/` and then renders — it does not merge. Running it in place would have destroyed the docs and the domain glossary.

## Decision

Scaffold into a throwaway `mktemp -d`, then `rsync -a` (WITHOUT `--delete`) the generated tree into the repo, excluding `.git/` and the kept paths.
The template emits none of the kept paths, so nothing collides. A git snapshot commit was taken first as a recovery point.

## Consequences

- Never run create-tauri-app's in-place `-f/--force` here; even after `git init`, only `.git/` survives the wipe.
- `tauri add` and later edits only touch Cargo.toml, lib.rs, package.json, tauri.conf.json, capabilities/*.json — never CONTEXT.md.
