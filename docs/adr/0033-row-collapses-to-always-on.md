# 33. A collapsed row shows always-on only; the footprint breakdown is disclosed per-row

## Status

Accepted.

## Context

DESIGN.md UX #1 shipped as: *"every row shows the full three-layer breakdown: always-on, on-invoke, and on-demand. No single blended number hides where the cost lives."*
On the 400px menu-bar panel that put three fixed 84px number columns plus a 22px action column into every row (`grid-template-columns: minmax(0, 1fr) 84px 84px 84px 22px`), so ~274px was spoken for before the skill name got anything.
The name column — the one variable-width thing on the row — was left with roughly 100px and ellipsized aggressively, and a real machine has skill names long enough to lose the tail (`open-gstack-browser`, `resolving-merge-conflicts`).

The three layers are not co-equal.
Always-on is the answer to the product's *headline* question — "which skills quietly tax my context on every request?" — and it is the only layer that is co-resident unconditionally.
On-invoke loads only when the skill fires; on-demand is a ceiling that is never even folded into a total (ADR 0017).
So the row was paying its scarcest resource, name width, to keep two situational figures permanently on screen.

## Decision

A collapsed row shows the always-on headline only.
Clicking the row discloses the **footprint breakdown**: all three layers, each named in full.
This knowingly amends UX #1 and UX #2; the anti-blending spirit of UX #1 is preserved, since nothing is blended — always-on stays an honest, un-mixed figure, and the other two layers are *disclosed*, not merged.

1. **Always-on is the headline (not a co-equal column).**
The collapsed row is `name · usage sub-line · always-on · caret`.
Attributed usage keeps its demoted sub-line under the name (ADR 0003): it answers the product's *second* question and sits below the name, costing no name width, so it is not hidden behind the disclosure.

2. **The breakdown repeats all three layers, labeled, with no total.**
Once the numbers leave the table grid, bare figures are meaningless, so each is self-labeled (Always-on / On-invoke / On-demand).
Always-on is repeated even though it is one line up, so the breakdown is a complete card rather than a two-line footnote to the row.
There is no sum: on-demand is a ceiling that never folds into the headline (ADR 0017), so the three do not add up to anything.

3. **Sorting drops to Name + Always-on.**
On-invoke and on-demand are no longer columns, so ranking the whole list by them would order it by figures no collapsed row shows.
They become inspect-only detail.
The header keeps the "Skill" sort word and reduces always-on to a bare sort chevron (no label): the number is the list's one headline figure and the breakdown names it in full, so the header carries only the control.
The pure `sortSkills` / `NUMERIC_VALUE` map still knows all four keys — only the on-invoke/on-demand *buttons* are gone — so restoring them later is a template change, not a logic one.

4. **Interaction: whole-row target, multi-open, persistent caret.**
The whole row is the click target and is itself the focusable, keyboard-operable disclosure control (Enter/Space, `aria-expanded` on the row); the caret is a visual indicator only, and the remove button stops propagation so it acts instead of toggling.
Rows are multi-open, reusing the exact `Set` semantics the per-repo sections already use (`expandedRepos`): a new pure `toggleInSet(set, key)` backs both, keyed by `skillKey(skill)` for rows.
State is all-collapsed on first load and survives a rescan (a still-present row keeps its open/closed state; a vanished skill drops its key).

5. **The row separator moves to the skill-group.**
A skill renders as its row plus, when open, the breakdown, plus, when managed, the manager-root row.
The one bottom border now belongs to the whole `.skill-group`, so those read as one unit — replacing the previous `.row.has-manager` border-shuffling, which only knew about the two-row case.

6. **The on-demand settle fade (ADR 0032) is dropped for the breakdown.**
That fade marked a pending "…" resolving to a number while the figure was permanently on screen.
Behind a disclosure the figure is usually off-screen when it resolves, and animating it on every *open* would be a fade with no meaning, so the on-demand value simply appears; the breakdown's own `slide` covers open/close.

## Alternatives considered

- **A wider panel.** The panel is anchored under the menu-bar item at 400px (ADR 0004 defers native window chrome); widening it to fit five columns comfortably is a bigger, separate change and still scales badly as names grow.
- **A compact all-three sub-line** (the three layers stacked under the name, like usage). Keeps everything visible but is dense, and still truncates the name whenever the sub-line is wider than it.
- **A sort menu** to preserve on-invoke/on-demand ranking. Real chrome on a 400px tray for a niche need (ranking the whole list by a ceiling); rejected as YAGNI, and reversible since the pure keys survive.

## Consequences

- The name column roughly doubles (~100px → ~260px); long skill names read whole.
- On-invoke and on-demand become inspect-only. A user comparing them across skills must open each row; acceptable, since they are situational detail, not ranking axes.
- New UI-context terms crystallize (Row, Footprint breakdown, Expanded/Collapsed), recorded in `src/CONTEXT.md` — the UI context CONTEXT-MAP.md had reserved but not yet created.
- The change is presentational: no Rust, no Tauri command, no `ScanReport` shape change. The pure `toggleInSet` seam is unit-tested; the rendered panel still needs a human eyeball via `pnpm tauri dev` (no headless browser is available on this machine).
