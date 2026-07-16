# 31. A plugin-root `SKILL.md` is named from its frontmatter, not its version-string directory

## Status

Accepted.
Closes the naming gap ADR 0030 recorded in its Consequences and deferred to issue #41: the one layout where keying `SkillId` on the directory basename is wrong rather than merely cosmetic.
Scoped by the docs, not by the manifests on disk, exactly as ADR 0030's own lesson demands.

## Context

skillmon keys `SkillId` on the directory basename for every skill kind.
For a plugin skill Claude Code does not always agree, and there is one layout where the disagreement is load-bearing: a `SKILL.md` sitting directly in the plugin's install root (`"skills": ["./"]`, or the auto single-skill layout of Claude Code v2.1.142+).
There the basename is the *install directory*, which for a marketplace install is a version string (`1.2.0`, `unknown`) that changes on every update, not a stable identity.

The gap was a no-op for all 55 plugin skills on the reference machine: every one has a frontmatter `name` equal to its directory basename, and none uses the plugin-root layout.
`plugin_with_only_a_root_skill_md_...` pinned the wrong name deliberately (ADR 0030) so the gap was recorded rather than asserted around.

The scope question the issue posed (plugin-only, or also personal/project? only the plugin root, or also a `skills/` subdirectory?) is not settled by the manifests, which agree either way.
It is settled by the docs, and the two relevant pages must be read together:

- **`plugins-reference.md` → "Path behavior rules"**: *"When a skill path points to a directory that contains a `SKILL.md` directly, for example `"skills": ["./"]` pointing to the plugin root, the frontmatter `name` field … determines the skill's invocation name … If `name` is not set … the directory basename is used as a fallback."* Read alone, "a directory that contains a `SKILL.md` directly" is ambiguous: it could include a declared `./skills/engineering/tdd`.
- **`skills.md` → "How a skill gets its command name"** resolves it. The table gives a plugin `skills/` subdirectory (`my-plugin/skills/review/SKILL.md`) the **directory name**, and a plugin-root `SKILL.md` the **frontmatter `name`, with the plugin directory name as a fallback**. Then, explicitly: *"The plugin-root case is the one place where `name` does set the command name, because there is no skill directory to take it from."*

So the rule is narrower than the seam the issue suggested (`discover_skill_at_dir`), which also handles a declared non-root path like `mattpocock-skills`'s `./skills/engineering/tdd`.
That path holds `SKILL.md` directly and routes through `discover_skill_at_dir`, but it is *not* the plugin root, so per `skills.md` it keeps its directory name.

This is not only a display concern.
Usage attribution joins a transcript's `attributionSkill` (the invocation name Claude Code records) to a skill's `SkillId.name` (`UsageKey`, ADR 0005/0024).
Keying a plugin-root skill on its version-string basename means the join never matches, so its attributed usage would read as untouched, and after a plugin update the old version-string tombstone would orphan too.
Aligning the identity to the invocation name is what makes both correct.

## Decision

A discovered skill's invocation name (the identity it is keyed, listed, and attributed by) is chosen by a `NamePolicy` resolved at discovery time:

- **`DirectoryBasename` (the default)** for every layout but one: personal, project, a plugin `skills/` depth-1 child, and a declared non-root path that happens to hold `SKILL.md` directly. The basename *is* the invocation name.
- **`FrontmatterName`** only for a plugin **install root** holding `SKILL.md` directly: the auto single-skill layout, and a declared `"skills": ["./"]` that resolves back to the root. The frontmatter `name` sets the identity, with the basename as the documented fallback.

The plugin adapter is the only caller that passes `FrontmatterName`, and only for the candidate directory that resolves to the install root.
A `"skills": ["./"]` path is normalized to the install directory so it behaves identically to the auto layout (same `dir_path`, same unmanaged manager-root resolution) rather than trailing a `.` component.
`discover_skills_in_dir` (every depth-1 scan) is always `DirectoryBasename`, since a directory's children are never a plugin root.

`SkillId::name` is now documented as the invocation name, not "the directory name."
`DiscoveredSkill::directory_name()` is renamed to `invocation_name()`, the honest name for what it returns and what every caller (listing lookup, the wanted-set memo, attribution, removal labels) actually uses.
`name_mismatch()` compares the *invocation* name against the frontmatter `name`: for a plugin-root skill the two are equal by construction, so it correctly reports no mismatch, while a personal directory `connect-chrome` declaring `open-gstack-browser` still fires.
Where a caller genuinely needs the on-disk folder (the `.agents` lock-key inversion, which searches the lock for the *sanitized folder name*), it reads `dir_path.file_name()` directly rather than the identity, so that correctness is a fact of the data, not a coincidence that a plugin-root skill's identity happens to equal its folder (it does not).

## Consequences

- No skill on the reference machine changes name: all 55 plugin skills already have `name == basename` and none uses the plugin-root layout, so this is a no-op for everything installed. The real-home test now asserts exactly that (`invocation_name == basename` for every discovered plugin skill), rather than assuming it.
- The fix is latent by design and asserted against synthetic fixtures: `plugin_with_only_a_root_skill_md_is_named_from_frontmatter_not_the_version_dir` now pins the *right* name, `declared_dot_slash_root_is_named_from_frontmatter` covers the explicit form, and `declared_non_root_direct_path_is_named_from_its_directory_not_frontmatter` locks in the "one place" boundary with a frontmatter that deliberately diverges.
- The naming rule stays **plugin-specific**. `skills.md` gives a personal or project skill the directory name unconditionally, and CLAUDE.md/DESIGN.md already document personal-skill depth-1 discovery; nothing says they follow the plugin-root rule, so they are deliberately untouched.
- The basename fallback for a nameless root `SKILL.md` is documented but unreachable: skillmon's frontmatter parser requires a non-empty `name`, so such a file warns as malformed before naming ever runs. No real plugin ships a nameless `SKILL.md`; making `name` optional is a broad change (`Frontmatter`, reconstruction, every fixture) with no live case, so it is left as a noted limitation, not built (YAGNI).
- `discover_skill_at_dir` gains a `NamePolicy` argument; its sole caller is the plugin adapter. `discover_skills_in_dir` is unchanged for callers (it fixes `DirectoryBasename` internally), so personal and project discovery are untouched.
- Plugin *removal* still goes through the `claude plugin` CLI (ADR 0007) and shares no code with this; a stable plugin-root identity only helps a future tombstone survive the version bumps that motivated the fix.

## Options considered

- **Apply `FrontmatterName` to the whole `discover_skill_at_dir` seam**, as the issue's "natural seam" wording suggests and `plugins-reference.md` alone permits. Rejected: `skills.md` is explicit that the plugin root is *the one place*, and the seam also serves declared non-root paths (`mattpocock-skills`'s 22), which must keep their directory names. A no-op on today's disk, but wrong in principle, and it would silently rename any future plugin whose nested skill declares a divergent `name`.
- **Generalize the rule to personal/project skills.** Rejected without evidence: the reference text is plugin-specific and gives non-plugin skills the directory name unconditionally. Personal-skill directory naming is a documented invariant elsewhere; changing it on the strength of a plugin-only sentence is exactly the over-reach the issue warned against.
- **Re-point `directory_name()` to `dir_path.file_name()` and add `invocation_name()` beside it.** Tempting, since it keeps a truthful "directory name" accessor, but it risks any test fixture whose `dir_path` and `id` name disagree (several set `dir_path: "/tmp/x"` with a real id name), and no caller actually needs the physical folder except the `.agents` inversion, which now sources it from `dir_path` directly. Renaming the one accessor to what it returns is simpler and carries no fixture risk.
- **Make `SkillId::name` carry both the basename and the declared name.** Rejected: the identity is a single stable key by design (usage, tombstones, removal all join on it). Two names on the id would push the choose-one decision into every consumer, which is precisely what resolving it once at discovery avoids.
