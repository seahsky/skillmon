import { describe, expect, it } from "vitest";
import {
  cascadeNote,
  formatBytes,
  isPurgeable,
  purgeSummaryMessage,
  rebuildWarning,
  reclaimableBytes,
  relativeAge,
  removalTitle,
  retentionDescription,
  revertedNote,
  sourceBlockedNote,
  sourceOptionLabel,
  trashUnitSummary,
  type RemovalPlanReport,
  type SourceOfferReport,
  type TrashUnitReport,
} from "./removal";
import type { SkillRef } from "./skills";

function ref(name: string): SkillRef {
  return { kind: "personal", name };
}

function plan(overrides: Partial<RemovalPlanReport> = {}): RemovalPlanReport {
  return {
    id: ref("vercel-react"),
    declaredName: "vercel-react",
    toolUninstall: false,
    dependents: [],
    entryPath: "/home/me/.claude/skills/vercel-react",
    source: null,
    rebuiltBy: null,
    ...overrides,
  };
}

function unit(overrides: Partial<TrashUnitReport> = {}): TrashUnitReport {
  return {
    id: 1,
    retention: "trashed",
    removedAtMillis: 1_000,
    primary: ref("vercel-react"),
    declaredName: "vercel-react",
    entryCount: 1,
    toolUninstall: false,
    bytes: 12_000,
    reverted: false,
    ...overrides,
  };
}

/** gstack's row, the one whose removal ADR 0027 refuses to call a skill removal. */
function gstackPlan(): RemovalPlanReport {
  return plan({
    id: ref("gstack"),
    declaredName: "gstack",
    toolUninstall: true,
    dependents: Array.from({ length: 46 }, (_, i) => ref(`shim-${i}`)),
    entryPath: "/home/me/.claude/skills/gstack",
  });
}

describe("removalTitle", () => {
  it("calls a row with dependents a tool uninstall, not a skill removal", () => {
    expect(removalTitle(gstackPlan())).toBe("Uninstall gstack?");
  });

  it("calls an ordinary row a removal", () => {
    expect(removalTitle(plan())).toBe("Remove vercel-react?");
  });
});

describe("cascadeNote", () => {
  // The floor, which ADR 0027 requires and which is not pedantry: gstack's own
  // setup links Codex, Factory, and OpenCode installs into the same checkout,
  // and skillmon scans none of those paths.
  it("states the dependent count as a floor, never as exhaustive", () => {
    const note = cascadeNote(gstackPlan()) ?? "";
    expect(note).toContain("46 other skills");
    expect(note).toContain("at least that many");
    expect(note).not.toMatch(/\ball\b|\bexactly\b|\bevery\b/);
  });

  it("says nothing when nothing cascades", () => {
    expect(cascadeNote(plan())).toBeNull();
  });

  it("reads naturally for a single dependent", () => {
    const note = cascadeNote(plan({ dependents: [ref("ship")] })) ?? "";
    expect(note).toContain("1 other skill resolve");
    expect(note).not.toContain("1 other skills");
  });
});

describe("rebuildWarning", () => {
  const managed = plan({
    declaredName: "ship",
    rebuiltBy: "/home/me/.claude/skills/gstack",
    source: { path: "/home/me/.claude/skills/gstack/ship", toolName: "gstack", blocked: "reset --hard" },
  });

  // ADR 0027's hazard: the disable is silently reverted, and skillmon's state
  // goes on claiming the skill is off while it is live in context.
  it("warns that a disabled managed entry will come back and be mislabeled", () => {
    const warning = rebuildWarning(managed, "disabled", false) ?? "";
    expect(warning).toContain("gstack");
    expect(warning).toContain("disabled while it is live");
  });

  it("warns more plainly for a delete, where the rebuild is at least visible", () => {
    const warning = rebuildWarning(managed, "trashed", false) ?? "";
    expect(warning).toContain("put this entry back");
    expect(warning).not.toContain("disabled while it is live");
  });

  // Taking the source is what makes the removal stick, so the rebuild warning
  // would be false.
  it("says nothing when the source is being removed too", () => {
    expect(rebuildWarning(managed, "trashed", true)).toBeNull();
  });

  it("says nothing for an unmanaged entry, which nothing puts back", () => {
    expect(rebuildWarning(plan(), "disabled", false)).toBeNull();
  });

  // An unknown manager still rebuilds; not recognizing the tool is no reason to
  // withhold the warning, only to phrase it by path.
  it("names the manager root when no tool is recognized", () => {
    const unknown = plan({ rebuiltBy: "/home/me/some-tool/skills", source: null });
    expect(rebuildWarning(unknown, "trashed", false)).toContain("/home/me/some-tool/skills");
  });
});

describe("source offers", () => {
  it("names the tool and the path the option reaches outside ~/.claude", () => {
    const source: SourceOfferReport = {
      path: "/home/me/.agents/skills/tdd",
      toolName: "the skills CLI (.agents)",
      blocked: null,
    };
    const label = sourceOptionLabel(source);
    expect(label).toContain("the skills CLI (.agents)");
    expect(label).toContain("/home/me/.agents/skills/tdd");
  });

  // The reason is the whole point of `can_remove_source` returning one: an
  // absent option that cannot explain itself reads as a bug.
  it("passes a tool's own refusal through verbatim", () => {
    const source: SourceOfferReport = {
      path: "/home/me/.claude/skills/gstack/ship",
      toolName: "gstack",
      blocked: "gstack rebuilds every skill it knows, and /gstack-upgrade runs git reset --hard.",
    };
    expect(sourceBlockedNote(source)).toBe(source.blocked);
  });

  it("has no note when the option is live", () => {
    expect(sourceBlockedNote({ path: "/x", toolName: "t", blocked: null })).toBeNull();
  });
});

describe("retentionDescription", () => {
  // Both intents are the same reversible move; only what may reclaim them
  // differs (ADR 0027). The copy must not imply delete destroys anything yet.
  it("does not claim a delete destroys anything before a purge", () => {
    expect(retentionDescription("trashed")).toContain("Nothing is deleted until you empty the trash");
    expect(retentionDescription("disabled")).toContain("indefinitely");
  });
});

describe("formatBytes", () => {
  // A gigabyte has to announce itself: explicit purge works only because the
  // figure is visible (ADR 0029).
  it("renders a gstack-sized trash unit as a gigabyte figure", () => {
    expect(formatBytes(1_100_000_000)).toBe("1.0 GB");
  });

  it("renders small entries without pretending to precision", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(2048)).toBe("2.0 KB");
    expect(formatBytes(20 * 1024)).toBe("20 KB");
  });
});

describe("relativeAge", () => {
  const now = 1_000_000_000_000;
  it("reads in the units a user thinks in", () => {
    expect(relativeAge(now, now)).toBe("just now");
    expect(relativeAge(now - 90_000, now)).toBe("2 minutes ago");
    expect(relativeAge(now - 3_600_000, now)).toBe("1 hour ago");
    expect(relativeAge(now - 86_400_000, now)).toBe("1 day ago");
    expect(relativeAge(now - 3 * 86_400_000, now)).toBe("3 days ago");
  });

  // Clocks move. A unit removed "in the future" by a few ms of skew must not
  // render a negative age.
  it("never renders a negative age", () => {
    expect(relativeAge(now + 5_000, now)).toBe("just now");
  });
});

describe("trashUnitSummary", () => {
  it("names a tool uninstall and its entry count, not just its size", () => {
    const summary = trashUnitSummary(unit({ toolUninstall: true, entryCount: 47, bytes: 1_100_000_000 }));
    expect(summary).toContain("Tool uninstall");
    expect(summary).toContain("47 entries");
    expect(summary).toContain("1.0 GB");
  });

  it("is just a size for an ordinary removal", () => {
    expect(trashUnitSummary(unit({ bytes: 2048 }))).toBe("2.0 KB");
  });
});

describe("revertedNote", () => {
  // The reconciliation ADR 0027 demands: the label has become a lie, and the
  // panel says so rather than keeping the claim.
  it("says a disabled skill is live again when its manager rebuilt it", () => {
    const note = revertedNote(unit({ retention: "disabled", reverted: true })) ?? "";
    expect(note).toContain("live again");
    expect(note).toContain("listed as disabled");
  });

  it("says a trashed unit's undo would overwrite what is there now", () => {
    const note = revertedNote(unit({ retention: "trashed", reverted: true })) ?? "";
    expect(note).toContain("overwrite");
    expect(note).toContain("will not");
  });

  it("says nothing for a unit whose origin is still clear", () => {
    expect(revertedNote(unit())).toBeNull();
  });
});

describe("purge affordances", () => {
  // Retained indefinitely means indefinitely: the affordance is absent, not
  // present-and-refusing (ADR 0029).
  it("never offers to reclaim a disabled unit", () => {
    expect(isPurgeable(unit({ retention: "disabled" }))).toBe(false);
    expect(isPurgeable(unit({ retention: "trashed" }))).toBe(true);
  });

  it("counts only trashed units toward the reclaimable total", () => {
    const units = [unit({ id: 1, bytes: 100 }), unit({ id: 2, retention: "disabled", bytes: 900 })];
    expect(reclaimableBytes(units)).toBe(100);
  });
});

describe("purgeSummaryMessage", () => {
  it("reports what was actually freed", () => {
    expect(purgeSummaryMessage({ units: 2, bytes: 2048, failed: 0 })).toBe("Reclaimed 2.0 KB from 2 removals.");
  });

  // A sweep that freed a gigabyte and failed on one tree did not fail — but it
  // did not fully succeed either, and it must not claim a clean sweep.
  it("does not claim a clean sweep when something could not be reclaimed", () => {
    const message = purgeSummaryMessage({ units: 1, bytes: 1024, failed: 1 });
    expect(message).toContain("Reclaimed 1.0 KB");
    expect(message).toContain("1 removal could not be reclaimed");
  });

  it("says so when there was nothing to reclaim", () => {
    expect(purgeSummaryMessage({ units: 0, bytes: 0, failed: 0 })).toBe("Nothing to reclaim.");
  });
});
