import { describe, expect, it } from "vitest";
import {
  alwaysOnDisplay,
  coResidentAlwaysOn,
  dependentsBadge,
  dependentsTitle,
  estimatedLayerCount,
  formatTokens,
  managerRootDisplay,
  managerRootTitle,
  groupByPlugin,
  groupProjectsByRepo,
  layerDisplay,
  mainSkills,
  normalizeApiKey,
  onDemandDisplay,
  repoBasename,
  scannedPaths,
  skillKey,
  skillNameTitle,
  sortSkills,
  toggleInSet,
  usageDisplay,
  usageTitle,
  type ScanReport,
  type SkillReport,
  type SkillRef,
  type UsageReport,
} from "./skills";

/**
 * Build a SkillReport with only the fields a test cares about overridden. The
 * identity defaults to a personal skill named `name`; pass `id` (via
 * `pluginId`/`projectId`) for the other two kinds.
 */
function makeSkill({
  name,
  id,
  ...overrides
}: Partial<Omit<SkillReport, "id">> & { name: string; id?: SkillRef }): SkillReport {
  return {
    id: id ?? { kind: "personal", name },
    live: true,
    alwaysOn: { tokens: 0, exact: true },
    alwaysOnText: "native",
    onInvoke: { tokens: 0, exact: true },
    onDemand: { tokens: 0, exact: true },
    usage: null,
    declaredName: name,
    nameMismatch: false,
    managerRoot: null,
    providesFor: 0,
    ...overrides,
  };
}

const pluginId = (name: string, plugin: string, marketplace = "official"): SkillRef => ({
  kind: "plugin",
  marketplace,
  plugin,
  name,
});

const projectId = (name: string, repoPath: string): SkillRef => ({ kind: "project", repoPath, name });

describe("sortSkills", () => {
  it("orders by always-on tokens, descending", () => {
    const skills = [
      makeSkill({ name: "small", alwaysOn: { tokens: 10, exact: true } }),
      makeSkill({ name: "big", alwaysOn: { tokens: 900, exact: true } }),
      makeSkill({ name: "mid", alwaysOn: { tokens: 100, exact: true } }),
    ];

    expect(sortSkills(skills).map((s) => s.id.name)).toEqual(["big", "mid", "small"]);
  });

  it("breaks ties by name ascending", () => {
    const skills = [
      makeSkill({ name: "zulu", alwaysOn: { tokens: 100, exact: true } }),
      makeSkill({ name: "alpha", alwaysOn: { tokens: 100, exact: true } }),
    ];

    expect(sortSkills(skills).map((s) => s.id.name)).toEqual(["alpha", "zulu"]);
  });

  it("does not mutate the input array", () => {
    const skills = [
      makeSkill({ name: "a", alwaysOn: { tokens: 1, exact: true } }),
      makeSkill({ name: "b", alwaysOn: { tokens: 2, exact: true } }),
    ];
    const before = skills.map((s) => s.id.name);

    sortSkills(skills);

    expect(skills.map((s) => s.id.name)).toEqual(before);
  });

  it("sorts ascending by a chosen numeric column", () => {
    const skills = [
      makeSkill({ name: "big", onInvoke: { tokens: 900, exact: true } }),
      makeSkill({ name: "small", onInvoke: { tokens: 10, exact: true } }),
      makeSkill({ name: "mid", onInvoke: { tokens: 100, exact: true } }),
    ];

    expect(sortSkills(skills, { column: "onInvoke", direction: "asc" }).map((s) => s.id.name)).toEqual([
      "small",
      "mid",
      "big",
    ]);
  });

  it("sorts by name ascending and descending", () => {
    const skills = [makeSkill({ name: "bravo" }), makeSkill({ name: "alpha" }), makeSkill({ name: "charlie" })];

    expect(sortSkills(skills, { column: "name", direction: "asc" }).map((s) => s.id.name)).toEqual([
      "alpha",
      "bravo",
      "charlie",
    ]);
    expect(sortSkills(skills, { column: "name", direction: "desc" }).map((s) => s.id.name)).toEqual([
      "charlie",
      "bravo",
      "alpha",
    ]);
  });

  it("sorts a pending (null) on-demand LAST in both directions, never to the top", () => {
    const skills = [
      makeSkill({ name: "pending", onDemand: null }),
      makeSkill({ name: "low", onDemand: { tokens: 10, exact: true } }),
      makeSkill({ name: "high", onDemand: { tokens: 900, exact: true } }),
    ];

    expect(sortSkills(skills, { column: "onDemand", direction: "desc" }).map((s) => s.id.name)).toEqual([
      "high",
      "low",
      "pending",
    ]);
    expect(sortSkills(skills, { column: "onDemand", direction: "asc" }).map((s) => s.id.name)).toEqual([
      "low",
      "high",
      "pending",
    ]);
  });

  it("sorts a null usage (untouched skill) LAST when sorting by usage work", () => {
    const withUsage = (name: string, work: number): SkillReport =>
      makeSkill({ name, usage: { work, cacheWrite: 0, cacheRead: 0, attributionSource: "native" } });
    const skills = [withUsage("busy", 5000), makeSkill({ name: "idle", usage: null }), withUsage("quiet", 100)];

    expect(sortSkills(skills, { column: "usageWork", direction: "desc" }).map((s) => s.id.name)).toEqual([
      "busy",
      "quiet",
      "idle",
    ]);
    expect(sortSkills(skills, { column: "usageWork", direction: "asc" }).map((s) => s.id.name)).toEqual([
      "quiet",
      "busy",
      "idle",
    ]);
  });
});

describe("skillKey", () => {
  it("distinguishes same-named plugins from different marketplaces", () => {
    const a = makeSkill({ name: "brainstorming", id: pluginId("brainstorming", "sp", "official") });
    const b = makeSkill({ name: "brainstorming", id: pluginId("brainstorming", "sp", "community") });

    expect(skillKey(a)).not.toBe(skillKey(b));
  });

  it("is stable for the same skill", () => {
    const s = makeSkill({ name: "grilling" });

    expect(skillKey(s)).toBe(skillKey({ ...s }));
  });

  it("distinguishes same-named project skills in different repos", () => {
    const a = makeSkill({ name: "deploy", id: projectId("deploy", "/repos/alpha") });
    const b = makeSkill({ name: "deploy", id: projectId("deploy", "/repos/beta") });

    expect(skillKey(a)).not.toBe(skillKey(b));
  });

  it("distinguishes a personal skill from a same-named skill of another kind", () => {
    const personal = makeSkill({ name: "ship" });
    const project = makeSkill({ name: "ship", id: projectId("ship", "/repos/alpha") });
    const plugin = makeSkill({ name: "ship", id: pluginId("ship", "gstack") });

    expect(new Set([skillKey(personal), skillKey(project), skillKey(plugin)]).size).toBe(3);
  });

  // The key is built by concatenation, so a separator that could occur inside a
  // name or a path would let two distinct identities collide. NUL cannot.
  it("does not collide when a name contains the key's other field values", () => {
    const a = makeSkill({ name: "sp", id: pluginId("sp", "a") });
    const b = makeSkill({ name: "a", id: pluginId("a", "sp") });

    expect(skillKey(a)).not.toBe(skillKey(b));
  });

  it("ignores fields outside the identity, so a re-scan keeps the row keyed the same", () => {
    const before = makeSkill({ name: "ship", alwaysOn: { tokens: 10, exact: false }, onDemand: null });
    const after = makeSkill({
      name: "ship",
      alwaysOn: { tokens: 4200, exact: true },
      onDemand: { tokens: 99, exact: true },
      usage: { work: 5, cacheWrite: 0, cacheRead: 0, attributionSource: "native" },
      managerRoot: "/home/me/.claude/skills/gstack",
    });

    expect(skillKey(before)).toBe(skillKey(after));
  });
});

describe("alwaysOnDisplay", () => {
  it("renders a not-listed skill as a certain 0 with no estimate/reconstructed framing", () => {
    const s = makeSkill({ name: "x", alwaysOnText: "notListed", alwaysOn: { tokens: 0, exact: true } });
    expect(alwaysOnDisplay(s)).toEqual({ text: "0", estimate: false, reconstructed: false, notListed: true });
  });

  it("marks an estimate with a ~ figure and the estimate flag", () => {
    const s = makeSkill({ name: "x", alwaysOnText: "native", alwaysOn: { tokens: 1234, exact: false } });
    expect(alwaysOnDisplay(s)).toEqual({ text: "~1,234", estimate: true, reconstructed: false, notListed: false });
  });

  it("flags always-on text reconstructed from frontmatter", () => {
    const s = makeSkill({ name: "x", alwaysOnText: "reconstructed", alwaysOn: { tokens: 500, exact: true } });
    expect(alwaysOnDisplay(s)).toEqual({ text: "500", estimate: false, reconstructed: true, notListed: false });
  });

  it("renders an exact native figure plainly", () => {
    const s = makeSkill({ name: "x", alwaysOnText: "native", alwaysOn: { tokens: 4200, exact: true } });
    expect(alwaysOnDisplay(s)).toEqual({ text: "4,200", estimate: false, reconstructed: false, notListed: false });
  });
});

describe("toggleInSet", () => {
  it("adds a key that is absent", () => {
    expect(toggleInSet(new Set(), "a")).toEqual(new Set(["a"]));
  });

  it("removes a key that is present", () => {
    expect(toggleInSet(new Set(["a", "b"]), "a")).toEqual(new Set(["b"]));
  });

  it("does not mutate the input set", () => {
    const before = new Set(["a"]);
    toggleInSet(before, "b");
    expect(before).toEqual(new Set(["a"]));
  });

  it("returns a new set instance, so a reassignment is always tracked", () => {
    const before = new Set(["a"]);
    expect(toggleInSet(before, "b")).not.toBe(before);
  });

  it("keeps independent keys open together (multi-open, not accordion)", () => {
    let open = new Set<string>();
    open = toggleInSet(open, "a");
    open = toggleInSet(open, "b");
    expect(open).toEqual(new Set(["a", "b"]));
  });
});

describe("skillNameTitle", () => {
  it("shows both names when the frontmatter name diverges from the directory", () => {
    // The real divergence on the reference machine (CONTEXT.md "Declared name").
    const skill = makeSkill({
      name: "connect-chrome",
      declaredName: "open-gstack-browser",
      nameMismatch: true,
    });

    const title = skillNameTitle(skill);
    expect(title).toContain("connect-chrome");
    expect(title).toContain("open-gstack-browser");
  });

  it("shows the directory name alone when the two agree", () => {
    expect(skillNameTitle(makeSkill({ name: "grilling" }))).toBe("grilling");
  });
});

describe("formatTokens", () => {
  it("groups thousands with commas", () => {
    expect(formatTokens(1234)).toBe("1,234");
    expect(formatTokens(1000000)).toBe("1,000,000");
  });

  it("leaves values under 1000 ungrouped", () => {
    expect(formatTokens(0)).toBe("0");
    expect(formatTokens(999)).toBe("999");
  });
});

describe("layerDisplay", () => {
  it("renders an exact count as a plain number", () => {
    expect(layerDisplay({ tokens: 1234, exact: true })).toBe("1,234");
  });

  it("prefixes an estimate with ~ so the two tiers never blend (ADR 0003/0006)", () => {
    expect(layerDisplay({ tokens: 1234, exact: false })).toBe("~1,234");
  });
});

describe("onDemandDisplay", () => {
  it("renders a pending (null) on-demand as an ellipsis, never 0 or ~0 (issue #11)", () => {
    const out = onDemandDisplay(null);
    expect(out).toBe("…");
    expect(out).not.toBe("0");
    expect(out).not.toBe("~0");
  });

  it("renders a resolved exact layer like layerDisplay", () => {
    expect(onDemandDisplay({ tokens: 1234, exact: true })).toBe(layerDisplay({ tokens: 1234, exact: true }));
    expect(onDemandDisplay({ tokens: 1234, exact: true })).toBe("1,234");
  });

  it("renders a resolved estimate layer with the ~ prefix", () => {
    expect(onDemandDisplay({ tokens: 1234, exact: false })).toBe("~1,234");
  });
});

describe("normalizeApiKey", () => {
  it("trims surrounding whitespace", () => {
    expect(normalizeApiKey("  sk-ant-abc  ")).toBe("sk-ant-abc");
  });

  it("returns an empty string for an all-whitespace input", () => {
    expect(normalizeApiKey("   \t\n")).toBe("");
  });
});

describe("usageDisplay", () => {
  it("renders nothing for an untouched skill, never ~0", () => {
    expect(usageDisplay(null)).toBe("");
  });

  // The display is identical whether the figure is native or reconstructed:
  // both are estimates rendered the same way (issue #12); the confidence badge
  // is a separate slice keyed off attributionSource, not the token text.
  const sources = ["native", "reconstructed"] as const;

  it.each(sources)(
    "shows ~work during this skill with cache-read segmented out, never blended or a currency (%s)",
    (src) => {
      const usage: UsageReport = { work: 1229, cacheWrite: 13781, cacheRead: 35154, attributionSource: src };
      const out = usageDisplay(usage);

      expect(out).toBe("~1.2k during this skill · ~35k cached");
      expect(out.startsWith("~1.2k during this skill")).toBe(true);
      expect(out).not.toContain("$");
    },
  );

  it.each(sources)("omits the cached segment when cache-read is zero (%s)", (src) => {
    expect(usageDisplay({ work: 500, cacheWrite: 0, cacheRead: 0, attributionSource: src })).toBe(
      "~500 during this skill",
    );
  });

  it.each(sources)(
    "usageTitle carries the full comma-grouped figures and the during-not-by framing (%s)",
    (src) => {
      const title = usageTitle({ work: 1229, cacheWrite: 13781, cacheRead: 35154, attributionSource: src });
      expect(title).toContain("~1,229 work tokens during this skill, not by it");
      expect(title).toContain("~35,154 cache-read");
      expect(title).toContain("~13,781 cache-write");
      expect(title).not.toContain("$");
    },
  );

  // The rolling-window label is independent of native/reconstructed, so a single
  // concrete source suffices (issue #14).
  it("labels the window when a rolling window is active", () => {
    const usage: UsageReport = { work: 35000, cacheWrite: 0, cacheRead: 0, attributionSource: "native" };
    expect(usageDisplay(usage, 24)).toBe("~35k during this skill · last 24h");
  });

  it("shows no window label for the all-time view", () => {
    const usage: UsageReport = { work: 35000, cacheWrite: 0, cacheRead: 0, attributionSource: "native" };
    expect(usageDisplay(usage, null)).toBe("~35k during this skill");
    // And the default (no window argument) stays all-time, so existing callers are unchanged.
    expect(usageDisplay(usage)).toBe("~35k during this skill");
  });
});

describe("estimatedLayerCount", () => {
  function makeReport(overrides: Partial<ScanReport>): ScanReport {
    return { skills: [], warnings: [], activeRepoPath: null, apiKeyPresent: true, usageWindowHours: null, ...overrides };
  }

  it("returns 0 when no key is present, even if every layer is an estimate", () => {
    const report = makeReport({
      apiKeyPresent: false,
      skills: [
        makeSkill({
          name: "a",
          alwaysOn: { tokens: 1, exact: false },
          onInvoke: { tokens: 2, exact: false },
          onDemand: { tokens: 3, exact: false },
        }),
      ],
    });

    expect(estimatedLayerCount(report)).toBe(0);
  });

  it("counts only non-exact layers across all skills when a key is present", () => {
    const report = makeReport({
      skills: [
        makeSkill({
          name: "a",
          alwaysOn: { tokens: 1, exact: true },
          onInvoke: { tokens: 2, exact: false },
          onDemand: { tokens: 0, exact: true },
        }),
        makeSkill({
          name: "b",
          alwaysOn: { tokens: 5, exact: false },
          onInvoke: { tokens: 6, exact: false },
          onDemand: { tokens: 7, exact: true },
        }),
      ],
    });

    // a: onInvoke estimate (1); b: alwaysOn + onInvoke estimates (2) => 3.
    expect(estimatedLayerCount(report)).toBe(3);
  });

  it("skips a pending (null) on-demand without throwing and without counting it (issue #11)", () => {
    const report = makeReport({
      skills: [
        makeSkill({
          name: "pending",
          alwaysOn: { tokens: 1, exact: false },
          onInvoke: { tokens: 2, exact: true },
          onDemand: null,
        }),
      ],
    });

    // Only the estimated always-on is counted; the null on-demand is neither
    // counted nor a source of a thrown `.exact` access.
    expect(() => estimatedLayerCount(report)).not.toThrow();
    expect(estimatedLayerCount(report)).toBe(1);
  });
});

describe("mainSkills", () => {
  it("keeps personal and plugin skills but drops project skills (they get per-repo sections)", () => {
    const skills = [
      makeSkill({ name: "personal-one" }),
      makeSkill({ name: "plug", id: pluginId("plug", "sp") }),
      makeSkill({ name: "proj", id: projectId("proj", "/repo/a") }),
    ];

    expect(mainSkills(skills).map((s) => s.id.name).sort()).toEqual(["personal-one", "plug"]);
  });
});

describe("groupByPlugin", () => {
  it("clusters personal under one group and each plugin under its own, with rows sorted", () => {
    const skills = [
      makeSkill({ name: "p-small", alwaysOn: { tokens: 10, exact: true } }),
      makeSkill({ name: "p-big", alwaysOn: { tokens: 900, exact: true } }),
      makeSkill({ name: "sp-a", id: pluginId("sp-a", "sp"), alwaysOn: { tokens: 500, exact: true } }),
      makeSkill({ name: "gs-a", id: pluginId("gs-a", "gs", "community"), alwaysOn: { tokens: 50, exact: true } }),
    ];

    const groups = groupByPlugin(skills);
    const byLabel = Object.fromEntries(groups.map((g) => [g.label, g.skills.map((s) => s.id.name)]));

    expect(byLabel["Personal"]).toEqual(["p-big", "p-small"]); // sorted always-on desc within group
    expect(byLabel["sp"]).toEqual(["sp-a"]);
    expect(byLabel["gs"]).toEqual(["gs-a"]);
    // Group order follows the sort: the strongest row (p-big=900) puts Personal first, then sp(500), then gs(50).
    expect(groups.map((g) => g.label)).toEqual(["Personal", "sp", "gs"]);
  });

  it("keeps same-named plugins from different marketplaces in separate groups", () => {
    const skills = [
      makeSkill({ name: "x", id: pluginId("x", "sp") }),
      makeSkill({ name: "y", id: pluginId("y", "sp", "community") }),
    ];

    expect(groupByPlugin(skills)).toHaveLength(2);
  });
});

describe("groupProjectsByRepo", () => {
  it("groups project skills by repo, active repo first, then others alphabetically", () => {
    const skills = [
      makeSkill({ name: "z-proj", id: projectId("z-proj", "/repos/zeta") }),
      makeSkill({ name: "a-proj", id: projectId("a-proj", "/repos/alpha") }),
      makeSkill({ name: "active-proj", id: projectId("active-proj", "/repos/active") }),
      makeSkill({ name: "personal" }),
    ];

    const sections = groupProjectsByRepo(skills, "/repos/active");

    expect(sections.map((s) => s.repoName)).toEqual(["active", "alpha", "zeta"]);
    expect(sections[0].isActive).toBe(true);
    expect(sections[1].isActive).toBe(false);
    // Personal skills never land in a repo section.
    expect(sections.flatMap((s) => s.skills.map((k) => k.id.name))).not.toContain("personal");
  });

  it("returns no sections when there are no project skills", () => {
    expect(groupProjectsByRepo([makeSkill({ name: "p" })], "/repos/active")).toEqual([]);
  });
});

describe("coResidentAlwaysOn", () => {
  it("sums personal + live plugins + only the active repo's project skills (DESIGN #5)", () => {
    const skills = [
      makeSkill({ name: "personal", alwaysOn: { tokens: 100, exact: true } }),
      makeSkill({ name: "live-plugin", id: pluginId("live-plugin", "sp"), live: true, alwaysOn: { tokens: 200, exact: true } }),
      makeSkill({ name: "dead-plugin", id: pluginId("dead-plugin", "sp"), live: false, alwaysOn: { tokens: 999, exact: true } }),
      makeSkill({ name: "active-proj", id: projectId("active-proj", "/repo/active"), alwaysOn: { tokens: 30, exact: true } }),
      makeSkill({ name: "other-proj", id: projectId("other-proj", "/repo/other"), alwaysOn: { tokens: 777, exact: true } }),
    ];

    // 100 + 200 + 30; the disabled plugin and the other repo are excluded.
    expect(coResidentAlwaysOn(skills, "/repo/active")).toEqual({ tokens: 330, exact: true });
  });

  it("a never-listed skill adds nothing, and does not drag the total to an estimate (issue #24)", () => {
    const skills = [
      makeSkill({ name: "listed", alwaysOn: { tokens: 100, exact: true } }),
      makeSkill({
        name: "grill-with-docs",
        alwaysOnText: "notListed",
        // Deliberately NOT the { tokens: 0, exact: true } the backend really
        // sends: the total must exclude a never-listed skill because it is not
        // in the listing, not because its numbers happen to be harmless.
        alwaysOn: { tokens: 999, exact: false },
      }),
    ];

    expect(coResidentAlwaysOn(skills, null)).toEqual({ tokens: 100, exact: true });
  });

  it("marks the total as an estimate if any contributing layer is an estimate (never blends tiers)", () => {
    const skills = [
      makeSkill({ name: "exact", alwaysOn: { tokens: 100, exact: true } }),
      makeSkill({ name: "estimate", alwaysOn: { tokens: 50, exact: false } }),
    ];

    const total = coResidentAlwaysOn(skills, null);
    expect(total.tokens).toBe(150);
    expect(total.exact).toBe(false);
    expect(layerDisplay(total)).toBe("~150");
  });
});

describe("repoBasename", () => {
  it("returns the last path segment, tolerating a trailing slash", () => {
    expect(repoBasename("/Users/me/Documents/skillmon")).toBe("skillmon");
    expect(repoBasename("/Users/me/repo/")).toBe("repo");
  });
});

describe("scannedPaths", () => {
  it("names the personal-skills root and plugin cache, plus the active repo when present", () => {
    expect(scannedPaths(null)).toEqual(["~/.claude/skills", "~/.claude/plugins/cache"]);
    expect(scannedPaths("/repo/active")).toEqual([
      "~/.claude/skills",
      "~/.claude/plugins/cache",
      "/repo/active/.claude/skills",
    ]);
  });
});

describe("managerRootDisplay", () => {
  it("shows a path that fits verbatim", () => {
    expect(managerRootDisplay("/opt/tools")).toBe("/opt/tools");
  });

  it("shows the manager roots on a real machine whole, at the length the backend sends", () => {
    // Absolute, home not collapsed: the exact strings that cross IPC.
    expect(managerRootDisplay("/Users/kyseah/.claude/skills/gstack")).toBe(
      "/Users/kyseah/.claude/skills/gstack",
    );
    expect(managerRootDisplay("/Users/kyseah/.agents/skills")).toBe("/Users/kyseah/.agents/skills");
  });

  it("shows a NESTED manager root whole, including the segment that names the manager", () => {
    // A manager root is the parent of the *resolved* skill dir, so a tool that
    // keeps its skills below the checkout root produces this shape, which is the
    // one the Rust integration test builds. Elide it hard and it reads
    // "…/engineering", naming a directory that is not the manager: the same
    // useless answer the basename rule gives, and the reason ADR 0026 shows
    // paths at all.
    expect(managerRootDisplay("/Users/kyseah/.claude/skills/gstack/skills/engineering")).toBe(
      "/Users/kyseah/.claude/skills/gstack/skills/engineering",
    );
  });

  it("elides whole leading segments, and marks it, when a path cannot fit", () => {
    const deep = "/Users/kyseah/Documents/GitHub/some-org/a-long-checkout-name/skills/engineering";
    const shown = managerRootDisplay(deep);

    expect(shown.startsWith("…/")).toBe(true);
    // Never the leading half: "/Users/kyseah/Doc…" is the same string for every
    // row on the machine, so a left-anchored clip would say nothing at all.
    expect(shown.endsWith("/skills/engineering")).toBe(true);
    expect(shown.length).toBeLessThanOrEqual(64);
  });

  it("still marks the elision when even the last segment overflows", () => {
    const long = `/a/${"x".repeat(80)}`;
    // No budget can fit it, so the marker stays and the CSS ellipsis clips the
    // rest, rather than returning a bare segment that reads as a whole path.
    expect(managerRootDisplay(long)).toBe(`…/${"x".repeat(80)}`);
  });

  it("never invents a name: what it shows is always a suffix of the real path", () => {
    for (const path of [
      "/Users/kyseah/.claude/skills/gstack",
      "/Users/kyseah/.agents/skills",
      "/Users/kyseah/.claude/skills/gstack/skills/engineering",
      "/very/deeply/nested/somewhere/else/entirely/that/keeps/going/and/going/skills",
    ]) {
      expect(path.endsWith(managerRootDisplay(path).replace(/^…\//, ""))).toBe(true);
    }
  });
});

describe("managerRootTitle", () => {
  it("carries the full path the row elided", () => {
    expect(managerRootTitle("/Users/kyseah/.claude/skills/gstack")).toContain(
      "/Users/kyseah/.claude/skills/gstack",
    );
  });
});

describe("dependentsBadge", () => {
  it("is absent for a row nothing resolves into", () => {
    expect(dependentsBadge(0)).toBeNull();
  });

  it("counts, and agrees with itself on the singular", () => {
    expect(dependentsBadge(1)).toBe("1 dependent");
    expect(dependentsBadge(46)).toBe("46 dependents");
  });
});

describe("dependentsTitle", () => {
  it("says removing the row takes the dependents with it", () => {
    expect(dependentsTitle(46)).toContain("46 skills resolve");
    expect(dependentsTitle(46)).toContain("removing it removes them too");
  });

  it("frames the count as a floor, never a total (ADR 0027)", () => {
    // skillmon scans Claude Code's paths alone, so a managing tool's entries for
    // other agents are dependents it cannot see. The tooltip must not claim
    // otherwise.
    expect(dependentsTitle(46)).toContain("At least");
  });

  it("agrees with itself on the singular", () => {
    expect(dependentsTitle(1)).toContain("1 skill resolves");
  });
});
