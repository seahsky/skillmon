# 28. The on-demand walk prunes non-reference content and guards against symlink cycles

## Status

Accepted.
Refines ADR 0017 (which fixed *how* an on-demand file is measured) by fixing *which* files are on-demand files at all.

## Context

`collect_files_recursive` walked a skill directory with zero exclusions, skipping only the skill's own `SKILL.md`.
A skill directory that is also a project checkout therefore had its entire tree counted as bundled reference files.
On the reference machine `~/.claude/skills/gstack` is 1.1 GB across 14,203 files: 704 MB of `node_modules/`, 311 MB of compiled binaries, a 60 MB `.git/` object store, and 46 nested `SKILL.md` directories (issue #26).

This is a correctness bug, not a cost problem.
The domain defines the on-demand layer as bundled reference files that load only if the body tells the agent to read them, reported as a ceiling.
No `SKILL.md` body instructs Claude to read `node_modules/.pnpm/`, a git object store, or a compiled binary, so the ceiling was measuring content that can never enter context and reporting it as a bound on content that might.
A nested skill's files were counted as the outer skill's references too, which they are not: that content reaches context (if at all) as the nested skill's own layers, loaded by the skill mechanism, never because the outer body told the agent to read it.
On the reference machine it is also a literal double-count, because gstack symlinks each of its 46 nested skills into `~/.claude/skills`, where discovery picks them up as their own rows (issue #30 counts them among the 71 personal skills).
That second fact is what makes it a double-count rather than merely a miscount, and it does not hold for every nesting shape: a plugin whose skills nest below depth 1 (mattpocock-skills nests `skills/<category>/<skill>/`) is not discovered at all today, so its bytes simply leave every row.

It is distinct from issue #11, which deferred this walk off the interactive path — deferring wrong work still yields a wrong number.

Separately, the walk tested `is_dir()`, which follows symlinks, and kept no record of where it had been.
A symlink pointing back at an ancestor was therefore unbounded recursion — a stack overflow rather than a slow scan.
No repro existed (gstack's 15 symlinks are not self-referential), so this is hardening, taken while the walk was open.

Reproduced on a gstack-shaped fixture assembled from real content (a real 26 MB plugin checkout with its real `node_modules`, a real git object store, 40 nested `SKILL.md` directories, and a directory symlink): the walk yielded 4,244 files totalling 26.5 MB.
After this decision it yields 63 files totalling 0.20 MB, and every real skill directory in the local plugin cache — none of which carries excludable content — produces a byte-identical file set to before.

## Decision

The walk prunes any directory whose contents cannot enter context through *this* skill:

- A deny-list of directory names, `.git` and `node_modules` — a VCS object store and a dependency tree are never reference material.
- Any directory containing its own `SKILL.md`. That subtree is another skill's, and its content reaches context as that skill's own three layers, not as a reference this skill's body reads. Where the nested skill is separately discovered — gstack's symlinked-in skills — it is a double-count as well.

Pruning happens before descent, so the excluded subtrees are never even enumerated — that is where the byte saving lives, not just the token correction.

The walk carries a set of canonicalized directory paths it has already visited and skips a repeat.
That bounds a symlink cycle and collapses content reachable through two paths to a single count.

Children are walked in sorted order with real directories before symlinked ones.
This exists only because the visited set introduces an alias tie-break: when a link and a real path reach the same directory, whichever is walked first wins, so `read_dir`'s arbitrary order would otherwise decide which path is recorded — the same content filed under a symlink's path on one scan and the real path on the next, churning `on_demand_signature` and missing the memo for no reason.
Walk order is otherwise invisible to the signature, which sorts its tuples before hashing; sorting real directories first simply makes the real path win.

## Consequences

- The on-demand ceiling now means what the glossary always said it meant, and the cold scan stops reading the excluded bulk.
- The "~216 MB bundled-file read" quoted in ADR 0022 and CLAUDE.md was measured with this bug present, so it counted non-reference content and is not a figure for what this walk now reads. It has not been re-measured: gstack is no longer installed on the machine at hand, so the 1.1 GB → reference-files-only claim in issue #26 stands on the issue's own measurement plus the fixture reproduction above, not on a fresh end-to-end number.
- **No `ON_DEMAND_LOGIC_VERSION` bump is needed**, unusually for a change to what the walk returns. The memo's signature is a hash over the `on_demand_files` set itself, so every skill whose set shrinks gets a new signature and misses naturally, and a skill with nothing to exclude keeps a memo that is still correct. The version exists for changes the signature cannot see; this is not one.
- The deny-list is a policy that will grow (`.venv`, `target`, `dist` are plausible). It is a deliberate deny-list rather than an allow-list of known reference extensions: an unknown file beside a `SKILL.md` is more likely a reference the body reads than not, so the default stays "counted".
- Deduplication is directory-level only. A single file reachable through two paths is still counted twice, matching the previous behaviour; only directories carry the cycle risk that forced the guard.
- The visited set goes slightly beyond the cycle guard the issue asked for: it also collapses a directory reachable through two *non-cyclic* aliases to one count. Deliberate — the alternative is knowingly counting the same bytes twice in a ceiling — but it is why the sorted walk above is needed at all.
- The walk still skips an unreadable directory silently, with no `DiscoveryWarning`, and the canonicalize call adds one more place that can happen. `list_on_demand_files` has no warning channel to report through, unlike `discover_skills_in_dir` right above it, so plumbing one is left as its own change rather than smuggled in here. A reference directory that vanishes from a ceiling is a quieter failure than a skill that vanishes from the list, but it is not nothing.
- Compiled binaries are not excluded by name. They already contribute no tokens (`on_demand_file_texts` drops non-UTF-8 files) and, on the reference machine, they sit inside the nested skill directories this decision already prunes.

## Options considered

- **Stop following symlinked directories entirely** — the simplest cycle fix, and it would make every recorded path real by construction. Rejected: it silently drops a reference directory that is a symlink to shared content, which is a real shape (gstack ships a directory symlink), and the issue asked for a guard, not a behaviour change.
- **A recursion-depth cap** — bounds the stack, but picks an arbitrary number and still walks a cycle to that depth. It treats the symptom; the visited set addresses the cause and pays for itself by deduplicating.
- **A gitignore-aware walk (the `ignore` crate)** — would prune `node_modules` and `.git` for free, but binds the ceiling to whatever a skill author happened to gitignore, which is a fact about their repo hygiene rather than about what Claude reads. It also adds a dependency to express a two-name list.
- **An allow-list of reference file extensions (`.md`, `.txt`, …)** — would exclude the binaries too, but silently drops any reference format not on the list, and the on-demand layer is a *ceiling*: erring toward counting is the honest direction.
