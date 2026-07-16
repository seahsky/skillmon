# 29. Explicit purge, and tombstones that outlive the bytes

## Status

Accepted; the engine landed with issue #28 and its callers and view with issue #31.
Implements DESIGN.md UX decision #6, completes the open end of ADR 0027 (`Trashed` = "eligible for purge" never said eligible *by what*), and discharges the cross-device `rename(2)` fallback ADR 0007 flagged and left unaddressed.

One amendment from issue #31: a trash **entry** may now carry the managing tool's copy of the same skill (`TrashedSource`), staged under the same unit, so one undo restores an entry and the content it points at together — see ADR 0027's Update for why it cannot be a unit of its own.
The retention-window auto-purge this ADR admits as a setting is **not** built: nothing self-empties, which is this ADR's own decision, and a toggle for an unimplemented behaviour would be worse than its absence.
Its absence remains the default.

## Context

ADR 0027 collapsed quarantine and trash into one reversible move carrying a retention intent, and left the reclaim end open.
Nothing reclaims a trash entry, so removal as designed is a disk leak whose first use is a gigabyte: trashing gstack moves **1.1 GB**, and a tool uninstall moves **47 entries** at once.

Two things are being retained, and they have opposite lifetimes.

- **The bytes.** An entry is normally tiny — a symlink, or a directory holding one — which is why ADR 0027 could make trash uniform at nil cost. The exception is the entry that dominates the disk: an entry that is also a manager root is the managing tool's entire checkout. The bytes exist only to buy an undo, and are worth a gigabyte only until the user is sure.
- **The history.** Attributed usage is already global and keyed by `message.id`, joined to a skill by attribution key (ADR 0024). Removing a skill deletes none of it. Retaining history therefore costs nothing and needs no new store; what is missing is a marker saying the row is gone. It is worth kilobytes forever.

DESIGN #6 requires that reinstalling restores continuity.
That is only meaningful if history survives the reclaim — otherwise the reclaim is what breaks continuity, and the feature is self-defeating.

## Decision

### Nothing self-empties; purge is explicit

A trash unit is reclaimed when the user says so, and never otherwise.
The removed view lists each unit with its reclaimable size and age, and offers a per-unit permanent delete plus an empty-trash action.

This keeps ADR 0007's "purge after confirmation" literally true, and it is the model users already hold from the OS trash.
The leak is answered by making it **visible** — the view carries a byte figure, so a gigabyte announces itself — rather than by a timer that deletes a gigabyte of the user's files behind their back.
A retention-window auto-purge is a legitimate convenience and is admitted as a **setting that ships off by default**; it lands with the view that hosts its toggle (issue #31), and until then its absence *is* the default.

`Disabled` is retained indefinitely and is not purgeable at all.
Empty-trash skips `Disabled` units rather than treating "remove everything staged" as one action, because that is the entire content of the retention intent: two labels over one code path (ADR 0027), differing only in what may reclaim them.

### A tombstone is not the trash, and outlives it

A tombstone is a `SkillId`-keyed row written when a unit is **trashed**, and it survives that unit's purge.
Purging drops the unit and its entries and keeps every tombstone: the bytes are the undo, the tombstone is the history, and reclaiming the first must not touch the second.

Disabling writes no tombstone.
A disabled skill is not removed — it is a row you can re-enable, and tombstoning it would file a live thing under "removed".

Continuity is restored by *rediscovery*, not by restore alone: a scan that finds a skill with a tombstone clears it.
That covers the case the store cannot see — the user reinstalling by hand, past skillmon entirely — and it makes restore's own tombstone clear a special case of one rule rather than a second mechanism.
Usage history needs no restoring because it was never removed, which is the point: the tombstone gates only whether the row is *listed*, so continuity is the absence of a deletion rather than the success of a recovery.

### A unit is atomic by precheck, then rollback

One removal is one unit with one undo, and a tool uninstall's 47 entries restore together or not at all (ADR 0027).
The filesystem offers no transaction across 47 paths, so atomicity is built in two steps:

1. **Precheck every entry before moving any.** A restore fails before it mutates anything if a stored entry is missing, or if an origin path is occupied. The occupied case is not hypothetical — it is exactly ADR 0027's recorded hazard, a managing tool having rebuilt the path while the entry sat in the trash, and it must fail loudly rather than clobber what the tool wrote.
2. **Roll back on partial failure.** If entry *k* fails after 0..*k* moved, those are moved back.

The precheck is what does the real work; the rollback covers the races the precheck cannot close.
Neither makes the operation a transaction, and this ADR does not claim one.

The database write and the filesystem moves are wrapped in one SQLite transaction that commits only after every move lands, so a crash can leave staged bytes with no unit row, but never a unit row with no bytes.
The stray directory that leaves behind is inert and is cleared when its id is next used.
The failure is deliberately biased: an orphan directory wastes disk, an orphan row would offer an undo that restores nothing.

### Cross-device: copy, fsync, swap, then unlink

`rename(2)` is atomic only within a filesystem.
On `ErrorKind::CrossesDevices` (stable since Rust 1.85, under the 1.89 MSRV) the move falls back to: copy into a staging slot **inside the destination's directory**, fsync it, rename it into place (same device, so atomic), then unlink the source.

The staging slot is nested one level down, at `<destination's dir>/.skillmon-partial/<name>`, and the nesting is not tidiness.
It has to sit inside the destination's directory to be on the destination's filesystem, which is what makes the swap atomic — a staging area anywhere else could be on a third device and merely move the problem.
But it must not be a *sibling* of the destination, because the destination is not always in the trash: a **restore** writes back into the scan root, so a sibling slot would be `~/.claude/skills/.tdd.skillmon-partial`.
Discovery filters nothing by name, so a crash between the copy and the swap would leave a fully discoverable bogus skill live in context, which no later purge would ever clean up — the unit's purge deletes its recorded stored path, not an abandoned staging slot.
Depth-2 nesting puts it out of reach of depth-1 personal-skill discovery on both sides of the move, satisfying both constraints at once.

Removing the source last is load-bearing.
A crash mid-fallback leaves the entry both live and staged — a duplicate, which the next scan simply sees as the skill still installed.
The opposite order risks losing it outright.

A symlink entry is **recreated as a symlink**, never resolved and copied.
skillmon removes the entry, never through it (ADR 0027), and a fallback that copied 1.1 GB of a symlink's target would reach through the entry precisely when the rename path did not — the same overreach ADR 0027 forbids, reintroduced as an error handler.

The fallback is not theoretical, and its trigger is worth naming: a unit's entries are all stored under the **primary** entry's root (`~/.claude/skillmon/removed/<unit>/` for personal and plugin skills, `<repo>/.claude/skillmon/removed/<unit>/` for project skills, preserving ADR 0007's project locality).
So a cascade that spans roots — a manager root under `~` with a dependent in a repo on another volume — crosses devices by construction.

### Sizes are measured at removal, and count what `rename` would move

An entry's byte figure is walked once, when it is removed, and stored.
Re-walking 1.1 GB on every panel open to render a number is not a trade worth making, and the figure cannot change afterwards: nothing writes into the trash.

This walk is **not** ADR 0028's on-demand walk and must not be confused with it.
ADR 0028 excludes `.git`, `node_modules`, and nested `SKILL.md` subtrees because they cannot enter *context*.
Here the question is what leaves the *disk*, and a 60 MB `.git` and a 704 MB `node_modules` are the overwhelming majority of the answer.
The exclusions that make one walk correct make the other a lie.
Symlinks are counted as the link, not the target, for the same reason the move preserves them.

## Consequences

The trash root sits outside every recursively watched path — `~/.claude/skillmon/` is not under `~/.claude/skills`, and `<repo>/.claude/skillmon/` is not under `<repo>/.claude/skills` — so staging bytes does not trigger the ADR 0019 watcher.
This answers the second half of issue #29 and is asserted by a test rather than left as a reading of the layout.
It does not answer the first half: removing the entry from the scan root is a write *inside* a watched tree and still needs self-write suppression.

Containment has two directions, and only one of them is about the trash root.
A restore's destination is the scan root, so anything the move touches transiently is inside the watched, discovered tree — which is why the staging slot above is nested rather than sibling.
The general rule the two cases share: **skillmon's transient artifacts must never be discoverable as skills**, and depth-1 discovery is the property that makes nesting sufficient without a name filter in `discovery/scan.rs`.
A future harness whose discovery is recursive would break that and would need the filter instead.

A skill can be tombstoned while its trash unit still holds the bytes, and a user can reinstall by hand in that window.
Rediscovery then clears the tombstone while the unit remains restorable, and the restore's precheck fails on the now-occupied origin.
That is the correct outcome — skillmon must not overwrite what the user just installed — and it falls out of the two rules rather than needing a third.

`provides_for` being a floor (ADR 0027) reaches the trash: a unit's byte figure covers the entries skillmon moved, so a manager root serving agents skillmon does not scan reclaims less than the tool's true footprint.
The view must not present the figure as the tool's total disk cost.

## Considered options

- **Auto-purge after a retention window, on by default.** The mainstream trash design, and it leaks nothing. Rejected: the undo window expires silently, and the thing it silently destroys is a gigabyte of the user's files. A product whose stated rule is "mutations are reversible" should not have irreversibility arrive on a timer.
- **Purge decided at removal time** — a size-aware dialog offering "keep for undo" or "delete now", so trash holds only what was chosen. Rejected as putting the decision at the worst moment: the user is least able to judge whether they want the undo before they have seen the consequence of the removal, which is the whole reason the undo exists.
- **Derive tombstones from trash units** rather than storing them. Tempting — a trashed unit's entries *are* the removed skills, so the table looks redundant. Rejected because it is only redundant until purge, and then it is wrong in the exact way DESIGN #6 forbids: reclaiming the bytes would erase the history, so a user could not both free the gigabyte and keep their totals honest.
- **Delete usage history on removal.** Rejected: it contradicts DESIGN #6, and it buys nothing — the rows are keyed by `message.id` and shared with sessions that have nothing to do with this skill.
