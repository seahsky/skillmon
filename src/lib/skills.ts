// Presentation-side mirror of the Rust IPC types (src-tauri/src/domain/report.rs)
// plus the pure logic the tray panel renders from. Kept free of Svelte and
// Tauri imports so it is unit-testable in isolation (CLAUDE.md verification gate).

/** One footprint layer: a token count and whether it is exact or an estimate. */
export interface LayerReport {
  tokens: number;
  /** `true` = exact (count_tokens), `false` = calibrated tiktoken estimate (ADR 0006). */
  exact: boolean;
}

/**
 * A skill's identity, mirroring the Rust `SkillRef` (serde-tagged on `kind`).
 * The panel's handle on a row: it holds this verbatim and hands it back to name
 * a row in a mutation, rather than reassembling a tuple the backend re-parses.
 *
 * A discriminated union, not a bag of nullables, so asking a personal skill for
 * its `marketplace` is a type error rather than a `null` to narrow away.
 */
export type SkillRef =
  | { kind: "personal"; name: string }
  | { kind: "project"; repoPath: string; name: string }
  | { kind: "plugin"; marketplace: string; plugin: string; name: string };

/** Mirrors the Rust `AlwaysOnTextKind`: where a skill's always-on text came
 * from (ADR 0016). `native` is the literal transcript-rendered line;
 * `reconstructed` is built from frontmatter because no session has listed the
 * skill yet; `notListed` means `disable-model-invocation` keeps it out of the
 * listing entirely, so its always-on cost is a certain zero, not a guess
 * (issue #24). Three states, so it deliberately isn't a boolean. */
export type AlwaysOnTextKind = "native" | "reconstructed" | "notListed";

/** Mirrors the Rust `AttributionSource`: `native` trusts Claude Code's own
 * attribution (issue #5); `reconstructed` is the version-gated walk over a
 * pre-attribution transcript (issue #12), a lower-confidence figure. */
export type AttributionSource = "native" | "reconstructed";

/**
 * Attributed session usage for a skill (ADR 0005): a demoted, fuzzy proxy,
 * never blended with the exact footprint (ADR 0003). `work` (input + output)
 * is the headline; `cacheRead` is shown separately and never folded in.
 */
export interface UsageReport {
  work: number;
  cacheWrite: number;
  cacheRead: number;
  attributionSource: AttributionSource;
}

/** One row the panel renders. Mirrors `SkillReport` (serde camelCase). */
export interface SkillReport {
  /** The row's identity, and the handle a mutation names it by (issue #31).
   * Carries the directory name plus whatever qualifies it: a repo, or a
   * marketplace and plugin. */
  id: SkillRef;
  live: boolean;
  alwaysOn: LayerReport;
  /** Where the always-on text came from (ADR 0016). `notListed` means the skill
   * never enters the listing, so `alwaysOn.tokens` is a certain zero. */
  alwaysOnText: AlwaysOnTextKind;
  onInvoke: LayerReport;
  /** `null` while the on-demand ceiling is still being computed off the
   * interactive scan (issue #11); the panel renders a pending affordance, not a
   * `0`. A resolved `{ tokens: 0 }` is the distinct "no bundled files" state. */
  onDemand: LayerReport | null;
  /** Attributed session usage, or `null` when no session touched this skill
   * (never a fabricated zero). Issue #5. */
  usage: UsageReport | null;
  /** The frontmatter `name:`. Shown alongside the directory name in `id` when
   * the two diverge, rather than the panel silently picking one. */
  declaredName: string;
  /** Whether `declaredName` diverges from the directory name. Decided by the
   * Rust domain, which owns what counts as divergence. */
  nameMismatch: boolean;
  /** The directory owning this skill's real content, or `null` when the skill
   * owns it itself (ADR 0026). A path, shown as one: no basename rule turns it
   * into a product name.
   *
   * `null` means unmanaged, NOT "safe to remove" — the row other skills resolve
   * into is itself unmanaged. Read it with `providesFor`, never alone. */
  managerRoot: string | null;
  /** How many discovered skills resolve into this one's directory. A floor,
   * never a total: skillmon counts only what it discovers, and it scans Claude
   * Code's paths alone (ADR 0027), so nothing rendered from this may claim to
   * be exhaustive. */
  providesFor: number;
}

/** Mirrors `ScanReport`. */
export interface ScanReport {
  skills: SkillReport[];
  warnings: string[];
  activeRepoPath: string | null;
  /** Whether an API key is configured. Only the presence crosses the IPC
   * boundary (issue #4); the key value never does. */
  apiKeyPresent: boolean;
  /** Which window the per-skill `usage` figures cover: `null` = all-time (the
   * default view, issue #5's cumulative numbers), `24` = the last 24h (issue
   * #14). The budget toast is independent of this and always 24h. */
  usageWindowHours: number | null;
}

/** Mirrors the Rust `SetKeyOutcome` (serde camelCase) returned by `set_api_key`. */
export type SetKeyOutcome = "stored" | "storedUnverified" | "rejected";

/** Mirrors the Rust `UsageSettings` (serde camelCase): the usage-toast config
 * round-tripped by `get_usage_settings` / `set_usage_settings` (issue #14). */
export interface UsageSettings {
  /** The rolling-24h attributed-work budget toast, on by default. */
  budgetEnabled: boolean;
  /** The attributed work-token ceiling per 24h. */
  budgetWorkTokens: number;
  /** Per-skill anomaly toasts, off by default. */
  anomalyEnabled: boolean;
}

/** A sortable column. `usageWork` sorts by attributed work tokens; a skill with
 * no usage (`null`) sorts last, like a pending on-demand. */
export type SortColumn = "name" | "alwaysOn" | "onInvoke" | "onDemand" | "usageWork";
export type SortDirection = "asc" | "desc";
export interface SortState {
  column: SortColumn;
  direction: SortDirection;
}

/** The panel's default order: always-on footprint descending (DESIGN.md UX #2). */
export const DEFAULT_SORT: SortState = { column: "alwaysOn", direction: "desc" };

/** The numeric value a skill sorts by, or `null` when the figure isn't known
 * yet: a pending on-demand ceiling (issue #11) or an untouched skill's usage.
 * A `null` always sorts last regardless of direction, so a not-yet-known number
 * never jumps to the top of a descending sort. `name` is compared as a string,
 * so it isn't in this map. */
const NUMERIC_VALUE: Record<Exclude<SortColumn, "name">, (s: SkillReport) => number | null> = {
  alwaysOn: (s) => s.alwaysOn.tokens,
  onInvoke: (s) => s.onInvoke.tokens,
  onDemand: (s) => s.onDemand?.tokens ?? null,
  usageWork: (s) => s.usage?.work ?? null,
};

/**
 * Sort skills by a column and direction (DESIGN.md UX #2: every layer column is
 * click-to-sort). Does not mutate the input. Ties always break by name ascending
 * so the order is deterministic across re-sorts. A `null` figure (pending
 * on-demand, untouched usage) sorts last in BOTH directions, never to the top.
 * The default sort (always-on descending) reproduces the shipped issue #1 order.
 */
export function sortSkills(skills: readonly SkillReport[], sort: SortState = DEFAULT_SORT): SkillReport[] {
  const { column, direction } = sort;
  const dir = direction === "asc" ? 1 : -1;

  if (column === "name") {
    return [...skills].sort((a, b) => dir * a.id.name.localeCompare(b.id.name));
  }

  const value = NUMERIC_VALUE[column];
  return [...skills].sort((a, b) => {
    const av = value(a);
    const bv = value(b);
    if (av === null && bv === null) return a.id.name.localeCompare(b.id.name);
    if (av === null) return 1; // a is unknown → after b
    if (bv === null) return -1; // b is unknown → after a
    const byValue = dir * (av - bv);
    return byValue !== 0 ? byValue : a.id.name.localeCompare(b.id.name);
  });
}

/**
 * A skill's identity flattened to a string, for the `{#each}` key — Svelte keys
 * must be primitives, so `skill.id` cannot be handed over as the object it is.
 * Anything that is not a keyed-each should take `skill.id` itself.
 *
 * One case per `SkillRef` variant, mirroring the domain identity in CONTEXT.md:
 * a plugin skill is `Plugin(marketplace, plugin, name)`, so `marketplace` is
 * part of the key — two same-named plugins from different marketplaces are
 * distinct rows.
 *
 * Joined on a NUL — written as an escape, never pasted in raw — which cannot
 * occur in a directory name or a path, so no two identities collide by
 * concatenation.
 */
export function skillKey(skill: SkillReport): string {
  const id = skill.id;
  switch (id.kind) {
    case "personal":
      return ["personal", id.name].join("\u0000");
    case "project":
      return ["project", id.repoPath, id.name].join("\u0000");
    case "plugin":
      return ["plugin", id.marketplace, id.plugin, id.name].join("\u0000");
  }
}

/**
 * Toggle a key's membership in a set, returning a NEW set — the input is never
 * mutated (a pure function must not touch its argument, and Svelte tracks the
 * reassignment rather than an in-place edit). Backs both of the panel's
 * disclosures with one multi-open semantics: the per-repo project sections
 * (`expandedRepos`) and the per-row footprint breakdown (`expandedSkills`, ADR
 * 0033). Keys are `repoPath` for repos and `skillKey(skill)` for rows.
 */
export function toggleInSet(set: ReadonlySet<string>, key: string): Set<string> {
  const next = new Set(set);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  return next;
}

/**
 * The hover text for a row's name. A skill whose frontmatter `name:` diverges
 * from its directory name is known to the user by either, so both are shown
 * rather than the panel silently picking one (CONTEXT.md "Declared name"). The
 * reference machine has a `connect-chrome` directory declaring
 * `open-gstack-browser`.
 *
 * The row still reads by directory name: that is the filesystem-stable identity
 * Claude Code renders, and what a mutation operates on (ADR 0016).
 */
export function skillNameTitle(skill: SkillReport): string {
  return skill.nameMismatch ? `${skill.id.name} · declared as "${skill.declaredName}"` : skill.id.name;
}

/** The always-co-resident skills — personal + plugin — that make up the main
 * list. Project skills are split out into per-repo sections (DESIGN.md UX #5),
 * so they are excluded here. Order is not guaranteed; callers sort. */
export function mainSkills(skills: readonly SkillReport[]): SkillReport[] {
  return skills.filter((s) => s.id.kind !== "project");
}

/** One header-and-rows cluster for the opt-in group-by-plugin view (DESIGN.md
 * UX #2): personal skills under one "Personal" group, each plugin under its own. */
export interface SkillGroup {
  key: string;
  label: string;
  kind: "personal" | "plugin";
  skills: SkillReport[];
}

/**
 * Group the main list (personal + plugin) under plugin/personal headers. Each
 * group's rows are sorted by `sort`; groups appear in the order their strongest
 * row appears in the sorted list, so the grouping never fights the chosen sort.
 * Marketplace is part of the group key so two same-named plugins from different
 * marketplaces stay distinct (matching `skillKey`'s identity).
 */
export function groupByPlugin(skills: readonly SkillReport[], sort: SortState = DEFAULT_SORT): SkillGroup[] {
  const groups = new Map<string, SkillGroup>();
  for (const skill of sortSkills(mainSkills(skills), sort)) {
    const id = skill.id;
    const key = id.kind === "plugin" ? `plugin:${id.marketplace}:${id.plugin}` : "personal";
    let group = groups.get(key);
    if (!group) {
      group = {
        key,
        label: id.kind === "plugin" ? id.plugin : "Personal",
        kind: id.kind === "plugin" ? "plugin" : "personal",
        skills: [],
      };
      groups.set(key, group);
    }
    group.skills.push(skill);
  }
  return [...groups.values()];
}

/** One repo's project skills, for the collapsed per-repo sections (DESIGN.md UX #5). */
export interface RepoSection {
  repoPath: string;
  repoName: string;
  /** The active repo, whose project skills are the only ones counted in the
   * global total (they are co-resident right now). */
  isActive: boolean;
  skills: SkillReport[];
}

/**
 * Group project skills by repo for the collapsed per-repo inventory sections.
 * The active repo comes first (its skills are co-resident and counted in the
 * total), the rest alphabetically by repo name. Each section's rows follow `sort`.
 */
export function groupProjectsByRepo(
  skills: readonly SkillReport[],
  activeRepoPath: string | null,
  sort: SortState = DEFAULT_SORT,
): RepoSection[] {
  const byRepo = new Map<string, SkillReport[]>();
  for (const skill of skills) {
    if (skill.id.kind !== "project") continue;
    const list = byRepo.get(skill.id.repoPath) ?? [];
    list.push(skill);
    byRepo.set(skill.id.repoPath, list);
  }
  return [...byRepo.entries()]
    .map(([repoPath, list]) => ({
      repoPath,
      repoName: repoBasename(repoPath),
      isActive: repoPath === activeRepoPath,
      skills: sortSkills(list, sort),
    }))
    .sort((a, b) => (a.isActive !== b.isActive ? (a.isActive ? -1 : 1) : a.repoName.localeCompare(b.repoName)));
}

/** Whether a skill is co-resident in context right now (DESIGN.md UX #5). */
function isCoResident(skill: SkillReport, activeRepoPath: string | null): boolean {
  switch (skill.id.kind) {
    case "personal":
      return true; // personal skills have no enable/disable; always co-resident
    case "plugin":
      return skill.live; // only plugins enabled in an applicable scope
    case "project":
      return skill.id.repoPath === activeRepoPath; // only the active repo's
  }
}

/**
 * The global always-on total (DESIGN.md UX #5): the sum of always-on tokens for
 * what is actually co-resident now — every personal skill, every LIVE plugin
 * skill, and ONLY the active repo's project skills. Other repos' project skills
 * are shown in their sections but never summed here. `exact` is true only when
 * every contributing layer is exact, so a mixed total is honestly `~`-marked as
 * an estimate (ADR 0003) when rendered through `layerDisplay`.
 *
 * A never-listed skill is skipped outright rather than left to add its zero
 * (issue #24). Adding it would reach the same total only for as long as the
 * backend keeps that zero `exact`; the day it didn't, a skill contributing
 * nothing would silently mark the whole total an estimate.
 */
export function coResidentAlwaysOn(skills: readonly SkillReport[], activeRepoPath: string | null): LayerReport {
  let tokens = 0;
  let exact = true;
  for (const skill of skills) {
    if (!isCoResident(skill, activeRepoPath)) continue;
    if (skill.alwaysOnText === "notListed") continue;
    tokens += skill.alwaysOn.tokens;
    if (!skill.alwaysOn.exact) exact = false;
  }
  return { tokens, exact };
}

/**
 * How much of a manager-root path the row shows before it elides.
 *
 * Sized to the sub-line's own full-row width (376px at the panel's 400px, at
 * 10px type), which is what the line spans the row to buy: a real manager root
 * is `…/skills/gstack/skills/engineering`-shaped, and squeezed into the name
 * column's ~112px there is no budget that keeps the segment naming the manager.
 * At this width the paths on a real machine fit whole and nothing elides at all.
 */
const MANAGER_ROOT_BUDGET = 64;

/**
 * A manager-root path, shortened only if it cannot fit the row (ADR 0026 shows
 * the path, never an invented product name).
 *
 * Elides whole leading segments and marks the elision with `…/`, so what's left
 * is a truncation of the path rather than a rule that renames it. Every naming
 * rule considered was wrong on real data: a basename gives `gstack` (right) but
 * also `skills`, from `~/.agents/skills` (useless). A budget set too low walks
 * into the same trap, since eliding hard enough turns
 * `…/skills/gstack/skills/engineering` into `…/engineering`, which names a
 * directory that is not the manager. So the fix is width, not cleverness about
 * which part to keep. `managerRootTitle` carries the whole path a hover away
 * regardless.
 */
export function managerRootDisplay(path: string): string {
  if (path.length <= MANAGER_ROOT_BUDGET) return path;
  const segments = path.split("/");
  for (let i = 1; i < segments.length; i++) {
    const tail = segments.slice(i).join("/");
    // +2 for the "…/" marker, which is part of what has to fit.
    if (tail.length + 2 <= MANAGER_ROOT_BUDGET) return `…/${tail}`;
  }
  // Even the last segment alone overflows: show it elided anyway and let the
  // CSS ellipsis clip the rest, rather than returning a path with no marker.
  return `…/${segments[segments.length - 1]}`;
}

/** The hover text for a manager-root line: the full path the row elides, plus
 * what being managed actually means for removing it. The managing tool, not the
 * user, decides whether this skill exists (ADR 0027). */
export function managerRootTitle(path: string): string {
  return `Content lives in ${path}. Whatever owns that directory puts this skill back when it next runs.`;
}

/**
 * The dependent-count badge (ADR 0026): how many discovered skills resolve into
 * this row's directory. `null` for a row nothing depends on, so the badge is
 * absent rather than a `0` on every ordinary skill.
 *
 * Shown beside the name, not demoted like the manager root, because it is the
 * fact that inverts the row's meaning: `managerRoot: null` alone reads as "safe
 * to delete" on the one entry whose removal takes 46 skills with it.
 */
export function dependentsBadge(providesFor: number): string | null {
  if (providesFor <= 0) return null;
  return providesFor === 1 ? "1 dependent" : `${providesFor} dependents`;
}

/**
 * The hover text for the dependent badge. States the floor honestly: skillmon
 * counts only the skills it discovers, and it scans Claude Code's paths alone,
 * so a managing tool's entries for other agents are real dependents it cannot
 * see (ADR 0027). Never claims the count is exhaustive.
 */
export function dependentsTitle(providesFor: number): string {
  const skills = providesFor === 1 ? "1 skill resolves" : `${providesFor} skills resolve`;
  return `${skills} into this directory, so removing it removes them too. At least that many: skillmon counts only the skills it can see.`;
}

/** The last path segment of a repo path — the name the user recognizes. */
export function repoBasename(path: string): string {
  const parts = path.replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || path;
}

/**
 * The paths a scan looks at, named for the empty state (DESIGN.md UX #7: name
 * the exact scanned paths so an empty panel is explainable, not mysterious). The
 * personal-skills root and the plugin cache are conventional under `~/.claude`;
 * the active repo's project-skills dir is appended when a repo is active.
 */
export function scannedPaths(activeRepoPath: string | null): string[] {
  const paths = ["~/.claude/skills", "~/.claude/plugins/cache"];
  if (activeRepoPath) paths.push(`${activeRepoPath}/.claude/skills`);
  return paths;
}

/**
 * Group an integer token count with thousands separators. Done with a regex
 * rather than `toLocaleString` so the output is identical regardless of the
 * runtime's ICU locale data.
 */
export function formatTokens(n: number): string {
  return Math.round(n)
    .toString()
    .replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/**
 * Render one footprint layer. An estimate is prefixed with `~` so it is never
 * mistaken for an exact count; the exact and estimate tiers are never blended
 * into one figure (ADR 0003, ADR 0006).
 */
export function layerDisplay(layer: LayerReport): string {
  const formatted = formatTokens(layer.tokens);
  return layer.exact ? formatted : `~${formatted}`;
}

/**
 * Render the on-demand layer, which may still be pending (issue #11). A pending
 * layer (`null`) shows an ellipsis, never a `0` or `~0` that would read as a
 * resolved ceiling; a resolved layer renders like any other.
 */
export function onDemandDisplay(layer: LayerReport | null): string {
  return layer === null ? "…" : layerDisplay(layer);
}

/** The always-on layer's rendered state, in one place so the collapsed row cell
 * and the breakdown line (ADR 0033) cannot drift. A not-listed skill shows a
 * certain `0` with none of the exact/estimate framing — the cost is known to be
 * nothing, not an imprecise count (issue #24); otherwise the figure renders like
 * any layer, carrying whether it is an estimate (`~`, ADR 0003) and whether its
 * always-on text was reconstructed from frontmatter rather than read from a
 * transcript (ADR 0016). */
export interface AlwaysOnDisplay {
  text: string;
  estimate: boolean;
  reconstructed: boolean;
  notListed: boolean;
}
export function alwaysOnDisplay(skill: SkillReport): AlwaysOnDisplay {
  if (skill.alwaysOnText === "notListed") {
    return { text: "0", estimate: false, reconstructed: false, notListed: true };
  }
  return {
    text: layerDisplay(skill.alwaysOn),
    estimate: !skill.alwaysOn.exact,
    reconstructed: skill.alwaysOnText === "reconstructed",
    notListed: false,
  };
}

/**
 * Trim surrounding whitespace from a pasted API key. Returns `""` for an
 * all-whitespace input, which drives the Save-disabled guard so an empty key
 * is never submitted (issue #4).
 */
export function normalizeApiKey(input: string): string {
  return input.trim();
}

/**
 * Compact token count for the demoted usage sub-line: `1.2k`, `35k`, `1.5M`.
 * Kept separate from `formatTokens` (comma-grouped) so the usage line reads as
 * a distinct, fuzzier proxy, not a headline figure.
 */
function compactTokens(n: number): string {
  const r = Math.round(n);
  if (r < 1000) return String(r);
  if (r < 1_000_000) return `${(r / 1000).toFixed(r < 10_000 ? 1 : 0)}k`;
  return `${(r / 1_000_000).toFixed(1)}M`;
}

/**
 * The demoted usage sub-line (issue #5, ADR 0003/0005): work tokens spent
 * *during* the skill, `~`-prefixed so it never reads as exact, with cache-read
 * ("context tax") shown separately and never folded into the work figure.
 * Returns `""` for an untouched skill so it renders no line (never `~0`), and
 * never emits a currency symbol.
 *
 * When `windowHours` is a positive number, the figure is windowed, so the line
 * says so ("· last 24h") to keep the number honest about its scope (issue #14).
 * Defaults to all-time (no label), so existing #5 callers are unchanged.
 */
export function usageDisplay(usage: UsageReport | null, windowHours: number | null = null): string {
  if (!usage) return "";
  const work = `~${compactTokens(usage.work)} during this skill`;
  const withCache = usage.cacheRead > 0 ? `${work} · ~${compactTokens(usage.cacheRead)} cached` : work;
  return windowHours && windowHours > 0 ? `${withCache} · last ${windowHours}h` : withCache;
}

/**
 * The hover text for the usage sub-line: the full (comma-grouped) figures, so
 * the compact line's clipped detail is recoverable, plus the honest framing
 * that these are tokens spent *during* the skill, not *by* it (ADR 0003).
 */
export function usageTitle(usage: UsageReport): string {
  const parts = [`~${formatTokens(usage.work)} work tokens during this skill, not by it`];
  if (usage.cacheRead > 0) parts.push(`~${formatTokens(usage.cacheRead)} cache-read (context tax, mostly cached)`);
  if (usage.cacheWrite > 0) parts.push(`~${formatTokens(usage.cacheWrite)} cache-write`);
  return parts.join(". ");
}

/**
 * How many footprint layers are still estimates while a key is present -- the
 * signal behind the "some counts couldn't be fetched exactly" banner (issue
 * #4). Returns 0 when no key is configured, since without a key every count is
 * expected to be an estimate and the banner would be noise. Counts the
 * non-exact layers (always-on, on-invoke, on-demand) across all skills.
 */
export function estimatedLayerCount(report: ScanReport): number {
  if (!report.apiKeyPresent) return 0;
  let count = 0;
  for (const skill of report.skills) {
    // A never-listed skill's always-on was never fetched, so it can't be a
    // count that failed to fetch exactly (issue #24).
    const layers = skill.alwaysOnText === "notListed" ? [skill.onInvoke] : [skill.alwaysOn, skill.onInvoke];
    for (const layer of layers) {
      if (!layer.exact) count += 1;
    }
    // A pending on-demand (`null`) is not a failed-exact count: skip it so it
    // neither throws on `.exact` nor misfires the "some counts couldn't be
    // fetched exactly" banner (issue #11).
    if (skill.onDemand && !skill.onDemand.exact) count += 1;
  }
  return count;
}
