# 21. Scan orchestration lives on the harness trait; the registry watcher only signals

## Context

Wiring discovery + footprint into a live app needed a home for two things: the scan orchestration (discover every skill, compute each skill's three-layer footprint, bundle the results into serializable IPC DTOs) and the debounced registry file-watcher over the ADR 0019 surfaces. Both placements had a non-obvious answer, and the obvious answer is wrong in each case.

## Decision

**Scan orchestration (`scan_all`) is a default method on the `HarnessAdapter` trait** in harness-neutral core, not a method on the concrete `ClaudeCodeAdapter`. It reads only through the trait's own `discover_skills` + `compute_footprint` and produces harness-neutral DTOs, so per ADR 0002 it belongs in generic core; a second harness adapter inherits it unchanged. The Claude Code adapter overrides it behind the same signature only for a batching optimisation (reading each transcript once per scan instead of once per skill).

**The registry watcher's debounce callback only emits a `registry-changed` signal; it never rescans itself.** The UI re-invokes `list_skills` in response, and `list_skills` re-syncs the watcher's own path set on the blocking pool.

## Consequences

- Putting `scan_all` on the trait makes the trait load-bearing rather than an unused abstraction, and keeps orchestration reusable across harnesses (ADR 0002).
- The watcher cannot rescan from inside its own callback without a borrow cycle — the callback would need `&mut` access to the watcher that owns it. Emitting and letting the caller act sidesteps that, and is correct on the merits too: enablement is read at session start (DESIGN.md), so a "your list may be stale" nudge with eventual consistency is the right model, not a live-state mirror.
- `list_skills` re-syncs the watcher on every call (a cheap, idempotent dir-listing + set diff), so a repo that gains a `.claude/skills/` dir after launch starts being watched without an app restart.

## Options considered

- **Orchestration on the concrete adapter** — simpler to write, but ties it to Claude Code and leaves the `HarnessAdapter` trait an unused abstraction; rejected (ADR 0002).
- **Watcher rescans directly in its callback** — a borrow cycle, and overkill for what is a confidence nudge rather than a correctness fix; rejected.
- **Scan orchestration on the trait, watcher emits and the command layer acts** — chosen.
