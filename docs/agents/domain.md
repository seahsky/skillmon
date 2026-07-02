# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

This repo is **multi-context**.

## Before exploring, read these

- **`CONTEXT-MAP.md`** at the repo root — it lists the contexts and points at one `CONTEXT.md` per context. Read each one relevant to the topic.
- **`src-tauri/CONTEXT.md`** — the Domain context glossary (skills, plugins, footprint, attributed usage, mutations). Most domain work touches this.
- **`src/CONTEXT.md`** — the UI context glossary. Created lazily; may not exist yet.
- **`docs/adr/`** — system-wide ADRs. Read the ones that touch the area you're about to work in. Context-scoped ADRs, when they exist, live under `src-tauri/docs/adr/` or `src/docs/adr/`.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when terms or decisions actually get resolved.

## File structure

Multi-context (a `CONTEXT-MAP.md` at the root):

```
/
├── CONTEXT-MAP.md
├── docs/adr/                          ← system-wide decisions
├── src-tauri/
│   └── CONTEXT.md                     ← Domain context
└── src/
    └── CONTEXT.md                     ← UI context (planned, lazy)
```

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in the relevant `CONTEXT.md`. Don't drift to synonyms the glossary explicitly lists under `_Avoid_` (e.g. say "attributed usage," not "cost" or "tokens used"; "quarantine," not "delete").

If the concept you need isn't in the glossary yet, that's a signal — either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0003 (both footprint and attributed usage) — but worth reopening because…_
