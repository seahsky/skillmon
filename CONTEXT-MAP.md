# skillmon — Context Map

skillmon is split into bounded contexts, each with its own ubiquitous language.
Read the context relevant to what you are working on.

## Contexts

- [Domain](./src-tauri/CONTEXT.md) — skills, plugins, footprint, attributed usage, and the mutations skillmon performs. Owns the harness-adapter trait and, for now, the Claude Code adapter's vocabulary.
- UI (`./src/CONTEXT.md`, planned) — the tray panel's presentation language (rows, columns, sort/group, badges, toasts, onboarding). Created lazily when panel-specific terms crystallize.

## Relationships

- **Domain → UI**: the UI renders domain concepts (footprint layers, attributed usage, plugin-locked state) through typed Tauri commands; it holds no domain logic.
- **Future adapters**: v1 folds the Claude Code adapter into the Domain context. When a second harness adapter lands, split each adapter into its own context under `src-tauri/adapters/<harness>/CONTEXT.md`, keeping harness-neutral terms in Domain (see ADR 0002).

## Decisions

System-wide ADRs live in [`docs/adr/`](./docs/adr/).
Context-specific ADRs, when they are needed, live under `src-tauri/docs/adr/` or `src/docs/adr/`.
