import { describe, expect, it } from "vitest";
import {
  estimatedLayerCount,
  formatTokens,
  layerDisplay,
  normalizeApiKey,
  skillKey,
  sortSkills,
  usageDisplay,
  usageTitle,
  type ScanReport,
  type SkillReport,
  type UsageReport,
} from "./skills";

/** Build a SkillReport with only the fields a test cares about overridden. */
function makeSkill(overrides: Partial<SkillReport> & { name: string }): SkillReport {
  return {
    kind: "personal",
    live: true,
    alwaysOn: { tokens: 0, exact: true },
    alwaysOnNative: true,
    onInvoke: { tokens: 0, exact: true },
    onDemand: { tokens: 0, exact: true },
    usage: null,
    repoPath: null,
    marketplace: null,
    plugin: null,
    ...overrides,
  };
}

describe("sortSkills", () => {
  it("orders by always-on tokens, descending", () => {
    const skills = [
      makeSkill({ name: "small", alwaysOn: { tokens: 10, exact: true } }),
      makeSkill({ name: "big", alwaysOn: { tokens: 900, exact: true } }),
      makeSkill({ name: "mid", alwaysOn: { tokens: 100, exact: true } }),
    ];

    expect(sortSkills(skills).map((s) => s.name)).toEqual(["big", "mid", "small"]);
  });

  it("breaks ties by name ascending", () => {
    const skills = [
      makeSkill({ name: "zulu", alwaysOn: { tokens: 100, exact: true } }),
      makeSkill({ name: "alpha", alwaysOn: { tokens: 100, exact: true } }),
    ];

    expect(sortSkills(skills).map((s) => s.name)).toEqual(["alpha", "zulu"]);
  });

  it("does not mutate the input array", () => {
    const skills = [
      makeSkill({ name: "a", alwaysOn: { tokens: 1, exact: true } }),
      makeSkill({ name: "b", alwaysOn: { tokens: 2, exact: true } }),
    ];
    const before = skills.map((s) => s.name);

    sortSkills(skills);

    expect(skills.map((s) => s.name)).toEqual(before);
  });
});

describe("skillKey", () => {
  it("distinguishes same-named plugins from different marketplaces", () => {
    const a = makeSkill({ name: "brainstorming", kind: "plugin", marketplace: "official", plugin: "sp" });
    const b = makeSkill({ name: "brainstorming", kind: "plugin", marketplace: "community", plugin: "sp" });

    expect(skillKey(a)).not.toBe(skillKey(b));
  });

  it("is stable for the same skill", () => {
    const s = makeSkill({ name: "grilling" });

    expect(skillKey(s)).toBe(skillKey({ ...s }));
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

describe("normalizeApiKey", () => {
  it("trims surrounding whitespace", () => {
    expect(normalizeApiKey("  sk-ant-abc  ")).toBe("sk-ant-abc");
  });

  it("returns an empty string for an all-whitespace input", () => {
    expect(normalizeApiKey("   \t\n")).toBe("");
  });
});

describe("usageDisplay", () => {
  const src = "native" as const;

  it("renders nothing for an untouched skill, never ~0", () => {
    expect(usageDisplay(null)).toBe("");
  });

  it("shows ~work during this skill with cache-read segmented out, never blended or a currency", () => {
    const usage: UsageReport = { work: 1229, cacheWrite: 13781, cacheRead: 35154, attributionSource: src };
    const out = usageDisplay(usage);

    expect(out).toBe("~1.2k during this skill · ~35k cached");
    expect(out.startsWith("~1.2k during this skill")).toBe(true);
    expect(out).not.toContain("$");
  });

  it("omits the cached segment when cache-read is zero", () => {
    expect(usageDisplay({ work: 500, cacheWrite: 0, cacheRead: 0, attributionSource: src })).toBe(
      "~500 during this skill",
    );
  });

  it("usageTitle carries the full comma-grouped figures and the during-not-by framing", () => {
    const title = usageTitle({ work: 1229, cacheWrite: 13781, cacheRead: 35154, attributionSource: src });
    expect(title).toContain("~1,229 work tokens during this skill, not by it");
    expect(title).toContain("~35,154 cache-read");
    expect(title).toContain("~13,781 cache-write");
    expect(title).not.toContain("$");
  });
});

describe("estimatedLayerCount", () => {
  function makeReport(overrides: Partial<ScanReport>): ScanReport {
    return { skills: [], warnings: [], activeRepoPath: null, apiKeyPresent: true, ...overrides };
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
});
