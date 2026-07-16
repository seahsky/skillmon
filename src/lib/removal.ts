// Presentation-side mirror of the Rust removal IPC types
// (src-tauri/src/domain/report.rs) plus the copy the confirm dialog and the
// removed view render. Kept free of Svelte and Tauri imports so it is
// unit-testable in isolation (CLAUDE.md verification gate).
//
// The copy lives here rather than in the markup because most of it is a claim
// about what will happen to the user's files, and those claims are the thing
// worth testing. "46 dependents" and "at least 46" are different promises.

import type { SkillRef } from "./skills";

/** Mirrors the Rust `Retention` (ADR 0027). The whole difference between
 * disabling a skill and deleting one: both are the same move to the same place,
 * and this is the only thing that tells them apart.
 *
 * `disabled` is kept indefinitely and is never purged; `trashed` is what the
 * user can later reclaim, and the only one that writes a tombstone. */
export type Retention = "disabled" | "trashed";

/** Mirrors `SourceOfferReport`: the managing tool's own copy of a skill,
 * offered for removal or refused with a reason. */
export interface SourceOfferReport {
  /** Where the content really lives. Shown in full — an option that reaches
   * outside `~/.claude` has to say where it reaches. */
  path: string;
  /** The tool that owns it, or `null` for a manager skillmon does not know. */
  toolName: string | null;
  /** `null` = the option is live. Otherwise the reason it is not, shown *in
   * place of* the option: a missing affordance that does not explain itself
   * reads as a bug (ADR 0027). */
  blocked: string | null;
}

/** Mirrors `RemovalPlanReport`: what removing a row would actually do,
 * worked out before anything moves. */
export interface RemovalPlanReport {
  id: SkillRef;
  declaredName: string;
  /** Whether this is a tool uninstall rather than a skill removal (ADR 0027). */
  toolUninstall: boolean;
  /** The skills that cascade. A floor, never a total — skillmon scans Claude
   * Code's paths alone, so a tool can have dependents it cannot see. Nothing
   * rendered from this may claim it is exhaustive. */
  dependents: SkillRef[];
  entryPath: string;
  /** `null` when the skill's content is its own entry, which is a different
   * statement from "you may not remove it". */
  source: SourceOfferReport | null;
  /** The manager root that will rebuild this entry, or `null` when nothing
   * puts it back (ADR 0027's recorded hazard). */
  rebuiltBy: string | null;
}

/** Mirrors `TrashUnitReport`: one staged removal in the removed view. */
export interface TrashUnitReport {
  id: number;
  retention: Retention;
  removedAtMillis: number;
  primary: SkillRef;
  declaredName: string;
  entryCount: number;
  toolUninstall: boolean;
  /** What purging this reclaims. A floor, never the managing tool's total disk
   * cost (ADR 0029). */
  bytes: number;
  /** Whether something has reappeared where this unit would restore to. On a
   * disabled unit that means its manager rebuilt the entry and the "disabled"
   * label is no longer true. Derived from the disk on every read, so it cannot
   * disagree with what a restore would do. */
  reverted: boolean;
}

/** Mirrors `TombstoneReport`: a removed skill whose bytes may already be gone.
 * The only handle the panel has on a purged row (DESIGN.md UX #6). */
export interface TombstoneReport {
  id: SkillRef;
  declaredName: string;
  removedAtMillis: number;
}

/** Mirrors `PurgeSummary`: what an empty-trash actually reclaimed, rather than
 * the figure the panel offered. */
export interface PurgeSummary {
  units: number;
  bytes: number;
  /** Units that could not be reclaimed and are still staged. A sweep that freed
   * 1.1 GB and failed on one tree did not fail — but it did not fully succeed
   * either, and the panel must be able to say so. */
  failed: number;
}

/**
 * The confirm dialog's title. A row with dependents is not a skill removal, it
 * is a tool uninstall, and it is labeled as one (ADR 0027) — because removing
 * `gstack` is not "removing a skill" in any sense the user would recognize.
 */
export function removalTitle(plan: RemovalPlanReport): string {
  return plan.toolUninstall ? `Uninstall ${plan.declaredName}?` : `Remove ${plan.declaredName}?`;
}

/**
 * What the removal takes with it, when it takes anything with it.
 *
 * States the count as a **floor**, never a total: skillmon scans Claude Code's
 * paths alone, and gstack's own setup links Codex, Factory, and OpenCode
 * installs into the same checkout — real dependents skillmon cannot see. So the
 * dialog says "at least", and never presents the number as exhaustive.
 */
export function cascadeNote(plan: RemovalPlanReport): string | null {
  const n = plan.dependents.length;
  if (n === 0) return null;
  const skills = n === 1 ? "1 other skill" : `${n} other skills`;
  return `${skills} resolve into ${plan.declaredName}, so this removes them too — at least that many, since skillmon only counts the skills it scans.`;
}

/**
 * ADR 0027's recorded hazard, said before the removal rather than discovered
 * after it: a managed entry is one its manager puts back.
 *
 * Only for a removal that leaves the manager's content in place. Taking the
 * source is precisely what makes the removal stick, so warning about a rebuild
 * then would be false.
 *
 * The wording is sharper for `disabled` because that is where the hazard bites:
 * a deleted-then-rebuilt skill is visibly back, while a disabled one leaves
 * skillmon claiming it is off while it is live in context.
 */
export function rebuildWarning(
  plan: RemovalPlanReport,
  retention: Retention,
  removingSource: boolean,
): string | null {
  if (!plan.rebuiltBy || removingSource) return null;
  const who = plan.source?.toolName ?? `whatever owns ${plan.rebuiltBy}`;
  return retention === "disabled"
    ? `${who} will put this entry back the next time it runs, and skillmon will go on showing it as disabled while it is live again. Check back here after you run it.`
    : `${who} will put this entry back the next time it runs.`;
}

/** The label for the opt-in that removes the managing tool's copy too. Names the
 * path, because this is the one option that reaches outside `~/.claude`. */
export function sourceOptionLabel(source: SourceOfferReport): string {
  const who = source.toolName ?? "the managing tool";
  return `Also remove ${who}'s copy at ${source.path}`;
}

/**
 * The two intents, as a labeled choice over one operation (ADR 0027).
 *
 * Both are the same reversible move; only what may later reclaim them differs.
 * The copy says so plainly rather than implying delete is more destructive than
 * it is — nothing is destroyed until the user purges it.
 */
export function retentionLabel(retention: Retention): string {
  return retention === "disabled" ? "Disable" : "Delete";
}

export function retentionDescription(retention: Retention): string {
  return retention === "disabled"
    ? "Moves it out of the scan root and keeps it indefinitely. Re-enable it any time."
    : "Moves it to skillmon's trash, where you can undo it or reclaim its disk space later. Nothing is deleted until you empty the trash.";
}

/**
 * Bytes, at the coarseness a human decides with. A reclaim figure exists to
 * answer "is this worth keeping around", and a gigabyte should announce itself
 * (ADR 0029: the leak is answered by making it visible, not by a timer).
 *
 * Powers of 1024 with SI-adjacent labels, matching what macOS's own trash and
 * every file manager the user has seen would say for the same directory.
 */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}

/**
 * How long ago a removal happened, in the units a user thinks in. "Now" is
 * passed in rather than read here: the core holds no wall clock (issue #14) and
 * neither does this, which is also what makes it testable without freezing time.
 */
export function relativeAge(removedAtMillis: number, nowMillis: number): string {
  const seconds = Math.max(0, Math.round((nowMillis - removedAtMillis) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return minutes === 1 ? "1 minute ago" : `${minutes} minutes ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
  const days = Math.round(hours / 24);
  return days === 1 ? "1 day ago" : `${days} days ago`;
}

/**
 * A removed-view row's one-line description: what it was, and how much of it
 * there is to give back.
 *
 * A tool uninstall says so, and names its entry count, because "47 entries" is
 * the whole difference between undoing a skill and undoing a toolchain.
 */
export function trashUnitSummary(unit: TrashUnitReport): string {
  const size = formatBytes(unit.bytes);
  if (!unit.toolUninstall) return size;
  const entries = `${unit.entryCount} entries`;
  return `Tool uninstall · ${entries} · ${size}`;
}

/**
 * The note on a unit whose origin has been rebuilt (ADR 0027's hazard, reached).
 *
 * Says the true thing for each intent. A disabled unit's label has become a
 * lie — the skill is live again — and that is the case the ADR demands
 * reconciling rather than continuing to claim. A trashed one is a plain
 * conflict: its undo cannot land, and skillmon will not overwrite what the tool
 * wrote.
 */
export function revertedNote(unit: TrashUnitReport): string | null {
  if (!unit.reverted) return null;
  return unit.retention === "disabled"
    ? "Its manager has put this back, so it is live again despite being listed as disabled. Remove its manager's copy too, or uninstall the tool, to make it stick."
    : "Something has been reinstalled where this would restore to, so undoing it would overwrite that. skillmon will not.";
}

/**
 * Whether the removed view may offer to reclaim this unit.
 *
 * `disabled` is retained indefinitely and is not purgeable at all — that is the
 * entire content of the retention intent (ADR 0029), so the affordance is absent
 * rather than present-and-refusing.
 */
export function isPurgeable(unit: TrashUnitReport): boolean {
  return unit.retention === "trashed";
}

/** What an empty-trash reclaimed, reported from what actually happened rather
 * than from what was offered. A partial sweep says so instead of claiming a
 * clean one. */
export function purgeSummaryMessage(summary: PurgeSummary): string {
  if (summary.units === 0 && summary.failed === 0) return "Nothing to reclaim.";
  const units = summary.units === 1 ? "1 removal" : `${summary.units} removals`;
  const freed = `Reclaimed ${formatBytes(summary.bytes)} from ${units}.`;
  if (summary.failed === 0) return freed;
  const failed = summary.failed === 1 ? "1 removal" : `${summary.failed} removals`;
  return `${freed} ${failed} could not be reclaimed and are still staged.`;
}

/** The total staged bytes the removed view can offer to reclaim: trashed units
 * only, since a disabled one is not garbage awaiting collection. */
export function reclaimableBytes(units: readonly TrashUnitReport[]): number {
  return units.filter(isPurgeable).reduce((sum, unit) => sum + unit.bytes, 0);
}
