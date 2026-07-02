# 17. On-invoke text is a confirmed deterministic template; on-demand stays raw-byte

## Context

ADR 0016 established that always-on footprint must measure the text Claude Code actually renders, not raw frontmatter, because that rendering carries unpredictable third-party decorations.
The same "raw source ≠ real context" question applies to the other two layers, but the evidence pointed to two different answers.

Inspecting a real `Skill` tool invocation in a transcript (the `ship` skill firing) showed the tool_result for the `Skill` tool_use is a fixed placeholder ("Launching skill: ship"); the actual body arrives as a separate user-turn text block prefixed with `Base directory for this skill: {absolute_path}\n\n` before the raw body, with frontmatter excluded.
The identical prefix appeared at the start of this very grilling session (`grill-with-docs`, invoked via slash command rather than the `Skill` tool), confirming the template is stable across both invocation paths, not something specific to one.
This is a known, simple, fully deterministic transformation — unlike always-on's decorations, there is nothing here that varies unpredictably by skill or by whatever tool manages it.

On-demand references are different again: they are loaded only if the body tells the agent to read them, and *how* the agent reads them is not fixed. Read (which renders `cat -n` line-numbered output, per its own tool description) and a Bash `cat`/`grep` each produce different actual context content from the same file, and which one happens is a runtime choice, not a property of the skill.

## Decision

On-invoke footprint = `count_tokens("Base directory for this skill: {path}\n\n" + body)`, computed directly every time, no transcript lookup needed — reconstruction here is exact, not a lower-confidence fallback, because the template is confirmed and has no third-party-variable component.
On-demand footprint stays the raw byte/token count of each bundled file, reported as a ceiling exactly as DESIGN.md already frames it — deliberately not chasing exact wrapper reconstruction, since which wrapper applies isn't knowable ahead of the model's own choice of tool, and the layer is explicitly non-headline already.

## Consequences

- Three layers, three different text-sourcing strategies, all serving one principle: measure what actually enters context, not raw source bytes, applied as far as it usefully can be — always-on (transcript-sourced, ADR 0016), on-invoke (deterministic template), on-demand (raw bytes, ceiling, principle knowingly not applied further).
- On-invoke footprint includes a small path-length-dependent term (the `Base directory for this skill: {path}` prefix) that is specific to where the skill happens to be installed, not just its content — two identical `SKILL.md` bodies at different paths have very slightly different on-invoke footprints. Correct, not a bug.
- No transcript is required to compute on-invoke footprint for a never-invoked skill, unlike always-on.

## Options considered

- **Treat on-invoke like always-on (transcript-sourced, template as fallback)** — unnecessary once the template was confirmed stable across both invocation paths; the transcript dependency would only add a data-availability gap (a never-invoked skill would show no on-invoke number) for no accuracy gain. Rejected.
- **Reconstruct on-demand via a guessed "most likely" wrapper (e.g. assume Read's line-numbering)** — the wrapper is a runtime choice, not a fixed fact about the skill; guessing one wrapper over another has no principled basis. Rejected; raw bytes chosen as the ceiling basis instead.
