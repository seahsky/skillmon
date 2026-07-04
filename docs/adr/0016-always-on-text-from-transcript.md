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

## Update (implementing the footprint counter plan)

Implementing the extraction against real transcripts on this machine found the attachment record is more structured than assumed here: `attachment.type == "skill_listing"`, and alongside `content` it carries a `names` array — the exact directory names, in the same order they appear in `content`. Two things this changes about the extraction mechanism (the decision above — read from transcript, reconstruct only as fallback — is unchanged):

- Anchor extraction on `names[i]` sequentially (find `- {names[i]}` at or after the cursor left by `names[i-1]`'s match, next name's start is this entry's end) rather than a bare `\n- ` scan. On today's real data the two approaches agree, but the `\n- ` heuristic would break silently on a future skill whose own description contains a markdown sub-list; the `names` array doesn't have that failure mode.
- Not every entry is `- {name}: {description}` — a skill with no frontmatter `description` renders as a bare `- {name}` with no colon and no trailing text (observed: `plan-tune`, `qa-only`, `review`, several others in the real listing). Extraction must not assume a colon is always present.

## Update (transcript search scope depends on skill type)

Which transcripts to search for a skill's rendered bullet depends on the skill's type, because a skill can only ever render in a session where it was co-resident:

- **Personal skills and user-scoped plugin skills** search across *all* known repos' transcripts (most-recently-modified first), since they can appear in any repo's session.
- **Project skills and project/local-scoped plugin skills** search *only their own repo's* transcripts, since that is the only place they can ever render.

The repo set comes from `discovery::transcript::enumerate_known_repos`, and the active-repo gate is the same one ADR 0015 uses for plugin liveness. A skill absent from every transcript in its scope falls back to the reconstructed line above.

## Update (the rendered bullet is now sourced through a persisted memo)

The batched scan resolves each skill's rendered bullet through a persisted per-transcript memo (`SqliteListingCache`, ADR 0022) instead of re-reading every transcript on every scan.
The native-first contract is unchanged: the bullet is still the most-recent rendered line in scope, per-repo scoping is byte-identical, and a never-rendered skill still falls back to the reconstructed line.
The Reconstructed-to-Native upgrade still happens on the next scan after a session renders the skill, because a render grows the transcript and the memo's `(mtime, size)` change forces a re-read.
