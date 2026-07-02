# 16. Always-on footprint text is read from a live transcript, not reconstructed from frontmatter

## Context

DESIGN.md and `src-tauri/CONTEXT.md` described the always-on layer as "the frontmatter (`name` + `description`)."
Grilling the footprint counter, real transcripts on this machine were inspected for the literal text Claude Code injects when listing available skills (found as `.attachment.content` records).
Two things contradicted the assumption: the rendered line uses the skill's **directory name**, not its frontmatter `name:` field (`connect-chrome`, whose frontmatter says `name: open-gstack-browser`, renders as `- connect-chrome: Launch GStack Browser…`); and the rendered line can carry more than name+description — `codex`'s entry has a trailing `Voice triggers (speech-to-text aliases): …` line that `domain-modeling`'s entry doesn't, evidently driven by a custom, non-standard frontmatter field.
There is no way to know generically which extra decorations a given skill-management convention might add to the rendered line, so hand-reconstructing "the template" is chasing a moving, third-party-influenced target.

## Decision

Source the always-on footprint text from a real transcript when one exists: find the most recent transcript that includes the skill, locate its rendered bullet by the `- {directory_name}: ` prefix in the attachment block, extract up to the next `\n- ` (or clear list-end boundary), and run `count_tokens` on that literal substring.
This is the native path, high confidence.
Only when no transcript has ever included the skill (just installed, no session run since) fall back to a reconstructed line built from the raw frontmatter's `name:` and `description:` fields alone, flagged `token_source = reconstructed` (lower confidence), and recomputed to native the first time a real session lists it.

## Consequences

- Mirrors the native-first/reconstructed-fallback shape already used for attributed usage (ADR 0005) — same domain pattern, trust what Claude Code actually wrote before reconstructing.
- A skill's always-on footprint can differ from a naive frontmatter-only estimate, sometimes by a full extra line (voice triggers or similar). That's exactly why the fallback is flagged, not silently trusted.
- Extracting one skill's bullet out of the shared attachment block requires boundary-parsing (the block lists every skill concatenated together); this parsing is Claude-Code-specific and lives in the harness adapter, not generic core (ADR 0002).
- DESIGN.md's "always-on layer" description needs correcting: it isn't fixed to two frontmatter fields, it's whatever Claude Code actually renders for that skill today.

## Options considered

- **Always reconstruct from raw frontmatter via a hand-maintained template** — simplest, but already wrong (misses decorations, uses the wrong name field) and would drift silently if the render template changes; kept only as the no-transcript-yet fallback.
- **Read the real rendered text from a live transcript, falling back to reconstruction only for never-yet-seen skills** — chosen.
