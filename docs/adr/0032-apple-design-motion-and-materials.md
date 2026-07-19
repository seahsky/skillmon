# 32. Apple-design motion and materials for the tray panel

## Status

Accepted.

## Context

The `apple-design` reference is overwhelmingly about gesture-driven touch interfaces: one-to-one drag tracking, release-velocity handoff, momentum projection, interruptible flings, and rubber-banding at boundaries.
The skillmon panel is none of those.
It is a 400x600 menu-bar dropdown whose content is a dense grid of tabular token figures, driven entirely by discrete clicks.
Most of the doc's machinery has no gesture surface to attach to here.

Three facts about the current build shaped the decision.
The panel has zero motion today: no `transition`, no `@keyframes`, no `will-change`, and no `prefers-reduced-motion` handling anywhere in `src/`, so every state change is an instant cut.
The window is `transparent: true` with `window-vibrancy` linked, but the CSS paints an opaque `--bg` over the whole surface, so that OS material is hidden.
The panel is a single persistent webview shown and hidden with `window.show()` / `window.hide()` (`lib.rs:520-529`), so `onMount` fires once at launch and never re-fires on open, and the webview cannot animate the OS window's own opacity or position.

Two capabilities matter.
Svelte 5 ships `svelte/motion` (`Spring` / `Tween`) plus `transition:` / `animate:`, so Apple's spring model is reachable with no new dependency.
ADR 0004 already deferred native window chrome (the `NSPopover` arrow and a system-owned flyout) past v1, which is where a true window-materialize animation would live.

## Decision

Import the parts of the doc that fit a discrete-tap panel and skip the gesture stack.
Imported: response (1), interruptibility where a spring is used (3), spatial consistency (7), frame-level smoothness (11), materials (12), reduced-motion and accessibility (14), typography (15), and the design foundations (16).
Skipped as inapplicable: one-to-one drag (2), velocity handoff (5), momentum projection (6), and rubber-banding (9).

1. Panel open and close stays instant.
An instant appearance is the most responsive possible outcome (1), and the webview cannot honestly animate the OS window frame.
A real window-materialize is deferred to the native bucket alongside ADR 0004, not faked inside the webview.

2. One calm motion token, CSS-first.
A single critically-damped curve (roughly Apple's damping 1.0 / response 0.3 to 0.4, no overshoot, about 200ms) is a CSS custom property used for every discrete transition.
A `svelte/motion` spring is used only where a value must resume from its live on-screen value under interruption, which on this panel is the removal modal alone.
No bounce or momentum token exists: the doc adds overshoot only when a gesture carried momentum (4), and nothing on this panel is flicked, so overshoot would be motion the doc itself argues against.

3. View-swaps are an iOS push and pop.
The table is the root; settings and removed are pushed in from the right and popped back out to the right, matching the `‹` back-arrow metaphor and satisfying the same-path-in-and-out rule (7) and familiarity (4).

4. Materials are translucent chrome over solid content.
The sticky topbar, controls bar, footer legend, and the modal become `backdrop-filter` layers with content scrolling under them (12); the data grid keeps a legible near-solid surface.
The hard 1px header divider becomes a scroll-edge fade.
The signature frosted-glass-over-desktop look is not adopted, because it trades the readability of the number grid, which is the panel's whole job, for decoration.

5. Only user-caused list motion animates.
A sort re-order animates rows to their new positions (Svelte `animate:flip`), the canonical case where motion helps track where a row went (8).
System-initiated rescan and registry refreshes stay instant, so rows never move under a reading user, and an on-demand `…` resolving to a number is a plain opacity fade with no count-up.

6. The remaining surfaces follow from the above.
The removal modal, already bottom-anchored, is a sheet that slides up from the bottom with a scrim fade and a blur-and-scale settle (12), dismissing along the same path (7); this is the single place the reserved spring is used, so a fast open-then-dismiss reverses cleanly from the live value (3).
The repo `<details>` sections animate their height on expand and collapse.
The row `⋯` affordance fades in rather than snapping, and every button and segment gets an instant press highlight plus a small `:active` scale (1).
A typography pass tightens heading tracking and leading and keeps the system font (15).

7. A reduced-motion, reduced-transparency, and increased-contrast baseline is mandatory, paired with every motion and material above (14).
`prefers-reduced-motion` drops all transforms to a short opacity fade of at most 120ms.
`prefers-reduced-transparency` makes the chrome solid and removes `backdrop-filter`.
`prefers-contrast: more` uses near-solid surfaces with defined borders.

## Considered options

A full fluid overhaul was rejected: the panel has almost no drag, flick, or swipe surface, so springs, velocity handoff, and momentum projection would animate from a velocity that is always zero.
A fully frosted panel background was rejected for the legibility reason in decision 4.
Animating every list change, including background rescans and a count-up on figures, was rejected because motion the user did not cause on a data grid erodes trust in the numbers (16) and risks strobing on a small surface (11).
A second bouncy token was rejected for the reason in decision 2.

## Consequences

No new dependency is added; motion is CSS transitions plus `svelte/motion` for the one modal.
The reduced-motion baseline is a hard requirement of shipping any of this, not a follow-up.
The "grows from the tray" moment does not exist until the ADR 0004 native work is picked up.
This is a UI-context decision recorded here because the `src/` context has not been split out and prior UI decisions (ADR 0004, ADR 0023) also live in `docs/adr/`.
No `src/CONTEXT.md` glossary was created: these are a design-decision record, and `CONTEXT.md` is meant to stay free of implementation detail.

## Update: implementation

Building it refined three of the decisions above. The behaviour each decision asked for is unchanged; these note where the mechanism differs from the letter.

The modal uses Svelte's built-in `transition:fly`/`transition:fade`, not a `svelte/motion` spring (decisions 2 and 6).
Svelte's transitions are interruptible and reverse from the live on-screen value when the sheet is dismissed mid-open, which is exactly the §3 property decision 2 reserved the spring for.
A hand-rolled spring would need an always-mounted-until-settled lifecycle for a difference that is imperceptible once overshoot is banned (decision 2), so it earned nothing.
No `svelte/motion` is imported; the motion is entirely `svelte/transition`, `svelte/animate`, and CSS.

The sheet's materialize is its slide-up plus the scrim fade over a static frosted material, not the "blur-and-scale settle" decision 6 wrote (§12).
On a sheet that rises the full height of the panel a 0.98-to-1 scale is invisible, and animating `backdrop-filter` blur is unreliable across engines, so both were dropped as cost without effect.

The one calm curve is ease-out-cubic, shared by the stylesheet (`--ease-calm`) and the JS transitions (`cubicOut`), so decision 2's "single curve" holds across the markup/stylesheet boundary.
A second, shorter duration token (`--dur-fast`, 120 ms) carries the §1 press and hover feedback that the 200 ms calm duration would make feel sluggish; the calm 200 ms remains the one transition duration.

Reduced motion shortens the opacity-based transitions to 120 ms rather than cutting them to nothing, and drops the transform-based motion (the sort flip, the press scale, the disclosure-marker rotation), per §14's "a gentler equivalent, not the absence of feedback."
The `flip-item` wrappers that carry `animate:flip` are marked `role="presentation"` so they do not break the `table`/`rowgroup`/`row` accessibility tree.
