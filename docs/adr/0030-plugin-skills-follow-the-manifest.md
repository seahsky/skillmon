# 30. Plugin skills follow the plugin's manifest, and the default scan it adds to

## Status

Accepted.
Implements the relocation rule DESIGN.md has always stated ("a plugin's own `plugin.json` may relocate its skills dir … so lock detection must read that field, not assume `skills/`") and which, it turns out, had never once executed.
Corrects issue #33's own proposed model on one point (`skills` *adds to* the default scan rather than replacing it), on the strength of the plugin reference the issue asked us to check.

## Context

skillmon discovered 15 of ~55 live plugin skills on the reference machine.
Two whole plugins resolved to zero: `mattpocock-skills` (22 skills, plus 6 spurious warnings) and `impeccable` (18).

Three independent defects compounded, all in `resolve_skills_dir`, and one shared mechanism hid all three: `fs::read_to_string(...).ok().and_then(|c| serde_json::from_str(&c).ok())`.

1. **The manifest path was wrong.** It read `<installPath>/plugin.json`. Every plugin on disk keeps its manifest at `<installPath>/.claude-plugin/plugin.json` — 11 of 11 that ship one, and the only location the plugin reference documents. `<installPath>/plugin.json` exists for zero plugins, so the read always failed and the relocation field never fired.
2. **`skills` is polymorphic and was typed `Option<String>`.** An array makes `from_str` fail for the *whole struct*, not just the field. On disk: 8 absent, 1 string (`impeccable`: `"./.claude/skills"`), 2 array (`mattpocock-skills`: 22 explicit paths; `ui-ux-pro-max`: 1).
3. **The walk was depth-1 from `skills/`.** `mattpocock-skills` nests `skills/<category>/<skill>/SKILL.md`, so its 6 category dirs each produced a `no readable SKILL.md` warning — the plugin read as malformed rather than nested. (ADR 0028 noted this in passing: "a plugin whose skills nest below depth 1 … is not discovered at all today". It is now.)

Each defect alone yields a plausible-looking `skills/` fallback. Together they made a silent zero indistinguishable from a plugin that ships no skills — which `serena` and `warp` genuinely do.

This is a correctness bug, not a coverage gap.
Always-on footprint is the headline (ADR 0003) and the global total sums discovered skills, so ~40 skills' listing lines entered context on every request while absent from that total.

**The fix is not "walk deeper."**
`mattpocock-skills` has 40 `SKILL.md` on disk and declares 22.
The undeclared 18 (`skills/deprecated/*`, `in-progress/*`, `misc/*`, `personal/*`) never enter context; a recursive walk would trade a 40-skill under-count for an 18-skill over-count, reporting skills that cannot load — the error class ADR 0028 exists to prevent.

The mechanism was confirmed rather than assumed: the default `skills/` scan is depth-1, mattpocock's declared skills sit at depth 2 under a category dir, so the default scan reaches *none* of them and the manifest is doing all the work.
That is why the declared set is exactly what loads.

Verified against an oracle independent of both skillmon and the manifests — a live session's own skill listing:

| plugin | manifest | live session shows | skillmon now |
| --- | --- | --- | --- |
| `impeccable` | string, 18 under it | 18 | 18 |
| `mattpocock-skills` | array, 22 paths | 9 model-listed + 13 slash-only | 22 |
| `superpowers` | absent | 14 (when enabled) | 14 |
| `frontend-design` | absent | 1 | 1 |
| `serena`, `warp` | absent, no `skills/` | 0 | 0 |

The mattpocock split is issue #24's `disable-model-invocation: true`: 13 of the 22 are slash-invokable but never listed to the model.
`/mattpocock-skills:implement` — the skill that wrote issue #33 — is one of them, which is why the listing shows 9 while the manifest declares 22, and why 22 is the right answer for a *footprint* tool.

## Decision

Discovery reads `<installPath>/.claude-plugin/plugin.json` and honors it.

- **`skills` is `string | array`**, both normalizing to a list of directories relative to the plugin root.
- **Declared directories add to the default `skills/` scan; they do not replace it.** `skills` is the one manifest field documented to extend rather than override its default. Issue #33 proposed replacement, and on all 11 plugins here the two models produce identical numbers — the default scan happens to contribute zero wherever anything is declared. They diverge for a plugin that ships both, which the documented model gets right. (The reference notes one exception, for a marketplace entry whose `source` resolves to the marketplace root; recognizing it needs marketplace source resolution, which discovery does not do, and no plugin on this machine takes that shape.)
- **Per candidate directory: a `SKILL.md` directly inside makes it one skill; otherwise its children are scanned depth-1.** Both stay depth-1 by construction. This is what makes `"./skills/engineering/implement"` one skill and `"./.claude/skills"` a directory of 18.
- **The documented single-skill layout** — a `SKILL.md` at the plugin root, no `skills/`, and no `skills` field (Claude Code v2.1.142+) — loads the root as one skill. It fires on zero plugins here; without it such a plugin would be invisible in precisely the way this issue is about.
- **Candidate directories are deduplicated by resolved path**, since a manifest may name the default explicitly (`"skills": ["./skills", "./extra"]`) — the documented way to keep a default while adding to it — and a skill discovered twice is counted twice in the headline.
- **A manifest that exists but will not read or parse raises a `DiscoveryWarning`** and is never collapsed into "declares nothing", on the same reasoning the adapter already applies to a corrupt `installed_plugins.json`. The discarded parse error is what let all three defects hide, so `Unreadable` is a distinct state from `Undeclared`: it still gets the default scan, but it must not trigger the root single-skill fallback, whose precondition ("no `skills` field") it cannot establish.
- **A declared path that is not on disk warns; a missing default `skills/` does not.** The manifest asserts the former; the latter is ordinary, since 3 of 11 plugins ship none.
- **A child directory holding no `SKILL.md` is reported only where children are meant to be skill entries** (`ChildDirs`). A plugin declaring paths *strictly under* a candidate directory is evidence that directory is a category tree, so its non-skill children are organizational, not malformed. Derived from what the plugin says about itself, not from a blanket rule about plugins — which keeps the warning alive where it still means something: a plugin that declares nothing and ships `skills/foo/` with no `SKILL.md` is still reported, because nothing explains that directory.

The manifest's location moves to `paths::plugin_manifest_path`, so the exact string that was the bug is asserted in one place (ADR 0002).

## Consequences

- The reference machine goes from 15 plugin skills to 55, with zero warnings. The global always-on total rises accordingly, and DESIGN.md UX #5's "what is actually co-resident now" starts being true.
- **A test fixture was the bug's accomplice and is now built through a helper.** The old test wrote `plugin.json` at the install root — the path discovery read — so relocation looked covered while never once executing against a real layout. Tests now write the manifest via a helper that spells `.claude-plugin/` once, so they cannot re-encode the location they exist to catch. This is the reason the real-`~/.claude` assertion below is not optional: no tempdir fixture can settle what plugin authors actually ship.
- `discover_skills_in_dir` gains a `ChildDirs` argument; personal and project discovery pass `AreSkillEntries`, preserving their contract exactly (a stale dir under `~/.claude/skills` is still reported — the user put it there and can act on it).
- The `skills` array's element paths are honored as written, and skillmon reports the declared set — *including* skills with `disable-model-invocation: true`, which are 13 of mattpocock's 22. That is right for a footprint tool: they load on invoke and carry on-invoke and on-demand cost. Issue #24 already ensures they are charged zero always-on rather than dropped.
- **Naming is deliberately unchanged and is a known gap.** The reference says a skill's invocation name comes from its frontmatter `name`, with the directory basename only as a fallback — and calls the basename actively wrong for the root single-skill layout, where it is a version string that changes on every update. skillmon keys `SkillId` on the directory basename for every skill kind. The two agree for all 55 skills on this machine, so this is a no-op today; `DiscoveredSkill::name_mismatch` (issue #27) already surfaces divergence rather than silently picking one. Changing it touches `SkillId` semantics, usage attribution keys, and removal's id→directory mapping, so it is its own issue, not a rider on a visibility fix.
- Plugin *removal* still goes through the `claude plugin` CLI (ADR 0007) and shares no code with this. More discovered plugin skills means more plugin-locked rows carrying the disabled affordance.
- The depth-1 rule per candidate directory is now load-bearing in two directions rather than one: too shallow returned 0 for mattpocock, too deep would return 40. Both errors are silent, which is why the real-home test asserts the property ("no plugin resolves to zero while declaring skills") rather than a count that a version bump invalidates.

## Options considered

- **Recursively walk `skills/`** — reaches mattpocock's declared 22 without parsing anything, and is the obvious reading of "the walk is depth-1". Rejected: it also reports the undeclared 18, which never enter context. The manifest is the only thing that distinguishes them, and a footprint tool that invents 18 skills is not better than one that misses 22 — it is the same bug with the sign flipped (ADR 0028).
- **Treat `skills` as replacing the default scan**, per issue #33's own three-case model. Rejected against the reference: `skills` is documented to add. Indistinguishable on today's disk, wrong for a plugin that ships `skills/` *and* declares more — and the issue explicitly asked that the docs settle this rather than 11 manifests.
- **Keep the silent `.ok()` fallback and just fix the path and the type.** Rejected: the fallback is the defect that outlived the other three. A plugin resolving to zero must be distinguishable from a plugin that ships zero, and only a warning does that.
- **Never warn about a plugin's non-skill child directories**, since a user cannot fix a plugin's internal layout anyway. Simpler than `ChildDirs`, and it satisfies the issue. Rejected: it reintroduces the silent zero for a genuinely broken plugin — the one property this ADR exists to protect. Classifying from the manifest's own evidence costs one predicate and keeps the signal.
- **Parse the marketplace-root `source` exception.** Deferred: it needs marketplace source resolution that discovery does not do, no installed plugin takes that shape, and inventing it unverified is how the original relocation field shipped un-executed for this long.
