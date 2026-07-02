# 10. JavaScript package manager — pnpm

Status: Accepted.

## Context

create-tauri-app and Tauri's tooling support npm, yarn, pnpm, and bun.
The choice is written into the scaffold (lockfile, `tauri add` invocation) and is annoying to switch later.

## Decision

Use **pnpm**. Fast, disk-efficient, strict.
`pnpm tauri add <plugin>` is the wiring path for official plugins; `pnpm tauri dev` / `pnpm build` drive the app.

## Consequences

- Commit exactly one lockfile (`pnpm-lock.yaml`); CI must use pnpm.
- pnpm 10 blocks dependency build scripts by default; `esbuild` is allowlisted via `pnpm.onlyBuiltDependencies` in package.json so the frontend builds.
