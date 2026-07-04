import { describe, expect, it } from "vitest";
import { formatTokens, layerDisplay, skillKey, sortSkills, type SkillReport } from "./skills";

/** Build a SkillReport with only the fields a test cares about overridden. */
function makeSkill(overrides: Partial<SkillReport> & { name: string }): SkillReport {
  return {
    kind: "personal",
    live: true,
    alwaysOn: { tokens: 0, exact: true },
    alwaysOnNative: true,
    onInvoke: { tokens: 0, exact: true },
    onDemand: { tokens: 0, exact: true },
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
