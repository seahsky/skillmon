# UI

The tray panel's presentation language: how the Domain's skills, footprint layers, and attributed usage are rendered, sorted, grouped, and disclosed.
This context holds no domain logic — it renders Domain concepts through typed Tauri commands ([`../CONTEXT-MAP.md`](../CONTEXT-MAP.md)).
Created when the per-row disclosure crystallized the first panel-specific terms (ADR 0033); grow it a term at a time as more UI language settles.

## Language

**Row**:
One skill's collapsed line in the list — its name and badges, the demoted usage sub-line beneath the name, the single always-on headline figure, and the disclosure caret.
A row shows only always-on; the other two footprint layers live in its footprint breakdown.
_Avoid_: entry (the Domain's on-disk thing — see `src-tauri/CONTEXT.md`; a row renders one, but is not one), item, cell (a cell is one column within a row).

**Footprint breakdown**:
The detail disclosed when a row is expanded: all three footprint layers (always-on, on-invoke, on-demand), each named in full, directly under the row and above the manager-root line.
It repeats always-on so it reads as a complete card, and shows no total — on-demand is a ceiling that never folds into the headline (ADR 0017).
_Avoid_: drawer (names the motion, not the content), detail panel, tooltip.

**Expanded / Collapsed**:
A row's disclosure state. Collapsed shows the always-on headline only; expanded shows the footprint breakdown beneath it.
Rows are multi-open (any number expanded at once), the state is all-collapsed on first load, and it survives a rescan.
The per-repo project sections are the same disclosure pattern over a different unit.
_Avoid_: open/closed for a row (kept for the repo sections), toggled.
