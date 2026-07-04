<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";
  import {
    layerDisplay,
    skillKey,
    sortSkills,
    type LayerReport,
    type ScanReport,
    type SkillReport,
  } from "$lib/skills";

  let report = $state<ScanReport | null>(null);
  let error = $state<string | null>(null);
  let loading = $state(true);

  // Rows are shown in the panel's default order: always-on footprint
  // descending (DESIGN.md UX decision #2). Click-to-sort on other columns is a
  // later slice, deliberately out of scope for issue #1.
  const rows = $derived(sortSkills(report?.skills ?? []));

  async function load() {
    loading = true;
    error = null;
    try {
      report = await invoke<ScanReport>("list_skills");
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  function repoName(path: string): string {
    const parts = path.replace(/\/+$/, "").split("/");
    return parts[parts.length - 1] || path;
  }

  // Each footprint cell carries a tooltip stating whether the number is exact
  // or a calibrated estimate, so the two tiers are never conflated (ADR 0003/0006).
  function layerTitle(layer: LayerReport): string {
    return layer.exact ? "Exact count" : "Calibrated tiktoken estimate, not exact";
  }

  function alwaysOnTitle(skill: SkillReport): string {
    const base = layerTitle(skill.alwaysOn);
    return skill.alwaysOnNative
      ? base
      : `${base}. Always-on text reconstructed from frontmatter; no session has listed this skill yet`;
  }

  function onDemandTitle(layer: LayerReport): string {
    const base = "Ceiling: raw size of bundled references, loaded only if the body reads them";
    return layer.exact ? base : `${base} (calibrated estimate)`;
  }

  onMount(() => {
    load();
    // The registry watcher (ADR 0019) fires this when a skill/plugin surface
    // changes; re-scan so the list doesn't go stale. Enablement is read at
    // session start, so this is a freshness nudge, not a live-state mirror.
    const unlisten = listen("registry-changed", () => load());
    return () => {
      unlisten.then((off) => off());
    };
  });
</script>

{#snippet layerCell(layer: LayerReport, title: string, reconstructed = false)}
  <div class="col num" role="cell" class:estimate={!layer.exact} class:reconstructed title={title}>
    {layerDisplay(layer)}
  </div>
{/snippet}

<main>
  <header class="topbar">
    <h1>Skills</h1>
    <div class="topbar-right">
      {#if report?.activeRepoPath}
        <span class="active-repo" title={report.activeRepoPath}>
          active repo: {repoName(report.activeRepoPath)}
        </span>
      {/if}
      <button class="rescan" onclick={load} disabled={loading} title="Rescan now">
        {loading ? "Scanning…" : "Rescan"}
      </button>
    </div>
  </header>

  {#if report?.warnings?.length}
    <ul class="warnings">
      {#each report.warnings as warning}
        <li>{warning}</li>
      {/each}
    </ul>
  {/if}

  {#if error}
    <div class="state error">
      <p>Couldn't load skills.</p>
      <code>{error}</code>
      <button onclick={load}>Try again</button>
    </div>
  {:else if loading && !report}
    <div class="state muted">Scanning skills…</div>
  {:else if rows.length === 0}
    <div class="state muted empty">
      <p>No skills found.</p>
      <button onclick={load}>Rescan</button>
    </div>
  {:else}
    <div class="table" role="table" aria-label="Installed skills">
      <div class="row header" role="row">
        <div class="col name" role="columnheader">Skill</div>
        <div class="col num sorted" role="columnheader" aria-sort="descending">
          <span class="ind">▼</span> Always-on
        </div>
        <div class="col num" role="columnheader">On-invoke</div>
        <div class="col num" role="columnheader">On-demand</div>
      </div>

      {#each rows as skill (skillKey(skill))}
        <div class="row" role="row" class:inactive={!skill.live}>
          <div class="col name" role="cell">
            <span class="skill-name" title={skill.name}>{skill.name}</span>
            {#if skill.kind === "plugin"}
              <span class="badge plugin" title="Plugin-locked: remove the whole plugin, not one skill">
                {skill.plugin ?? "plugin"}
              </span>
            {:else if skill.kind === "project" && skill.repoPath}
              <span class="badge project" title={skill.repoPath}>{repoName(skill.repoPath)}</span>
            {/if}
            {#if !skill.live}
              <span class="badge inactive-badge" title="Not live in the active context (contributes zero live footprint)">inactive</span>
            {/if}
          </div>

          {@render layerCell(skill.alwaysOn, alwaysOnTitle(skill), !skill.alwaysOnNative)}
          {@render layerCell(skill.onInvoke, layerTitle(skill.onInvoke))}
          {@render layerCell(skill.onDemand, onDemandTitle(skill.onDemand))}
        </div>
      {/each}
    </div>

    <footer class="legend">
      <span><span class="swatch estimate">~</span> calibrated estimate (add an API key for exact counts)</span>
      <span>On-demand is a ceiling.</span>
    </footer>
  {/if}
</main>

<style>
  :root {
    --bg: #f7f7f8;
    --fg: #1c1c1e;
    --muted: #6b6b70;
    --faint: #98989d;
    --line: #e2e2e5;
    --accent: #396cd8;
    --badge-bg: #ececef;
    --estimate-fg: #8a6d00;
    font-family:
      -apple-system, BlinkMacSystemFont, "Segoe UI", Inter, system-ui, sans-serif;
    font-size: 13px;
    line-height: 1.4;
    color: var(--fg);
  }

  main {
    background: var(--bg);
    min-height: 100vh;
    padding: 8px 0 0;
    display: flex;
    flex-direction: column;
  }

  .topbar {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 8px;
    padding: 0 12px 8px;
    border-bottom: 1px solid var(--line);
  }

  h1 {
    font-size: 14px;
    font-weight: 600;
    margin: 0;
  }

  .topbar-right {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .active-repo {
    color: var(--muted);
    font-size: 11px;
    max-width: 140px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  button {
    font-family: inherit;
    font-size: 12px;
    color: var(--fg);
    background: transparent;
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 3px 8px;
    cursor: pointer;
  }
  button:hover:not(:disabled) {
    border-color: var(--accent);
    color: var(--accent);
  }
  button:disabled {
    color: var(--faint);
    cursor: default;
  }

  .warnings {
    margin: 8px 12px 0;
    padding: 6px 10px;
    list-style: none;
    background: #fff8e6;
    border: 1px solid #f0e0a8;
    border-radius: 6px;
    color: #6b5900;
    font-size: 11px;
  }
  .warnings li + li {
    margin-top: 2px;
  }

  .state {
    padding: 32px 16px;
    text-align: center;
  }
  .state.muted {
    color: var(--muted);
  }
  .state.error code {
    display: block;
    margin: 8px auto;
    max-width: 90%;
    color: #b3261e;
    font-size: 11px;
    word-break: break-word;
  }
  .state button {
    margin-top: 8px;
  }

  .table {
    display: flex;
    flex-direction: column;
  }

  .row {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 84px 84px 84px;
    align-items: center;
    gap: 4px;
    padding: 5px 12px;
    border-bottom: 1px solid var(--line);
  }
  .row.header {
    position: sticky;
    top: 0;
    background: var(--bg);
    padding-top: 7px;
    padding-bottom: 7px;
    color: var(--muted);
    font-size: 11px;
    font-weight: 600;
  }
  .row:not(.header):hover {
    background: rgba(57, 108, 216, 0.06);
  }
  .row.inactive {
    opacity: 0.55;
  }

  .col {
    min-width: 0;
  }
  .col.name {
    display: flex;
    align-items: center;
    gap: 6px;
    overflow: hidden;
  }
  .skill-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 500;
  }
  .col.num {
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-feature-settings: "tnum";
    white-space: nowrap;
  }
  .row.header .col.sorted {
    color: var(--fg);
  }
  .ind {
    font-size: 9px;
    color: var(--accent);
  }

  .badge {
    flex: none;
    font-size: 10px;
    padding: 1px 6px;
    border-radius: 999px;
    background: var(--badge-bg);
    color: var(--muted);
    max-width: 96px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .badge.plugin {
    background: #e8effb;
    color: #2f5bb7;
  }
  .badge.project {
    background: #eaf6ec;
    color: #2f7d3a;
  }
  .badge.inactive-badge {
    background: transparent;
    border: 1px solid var(--line);
  }

  /* An estimate is muted and marked; it never blends with an exact count. */
  .col.num.estimate {
    color: var(--estimate-fg);
  }
  /* Always-on text reconstructed from frontmatter (no transcript yet). */
  .col.num.reconstructed {
    text-decoration: underline dotted;
    text-underline-offset: 3px;
  }

  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    padding: 8px 12px 10px;
    color: var(--faint);
    font-size: 10px;
  }
  .swatch.estimate {
    color: var(--estimate-fg);
    font-weight: 600;
  }

  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #1e1e20;
      --fg: #f2f2f4;
      --muted: #a0a0a6;
      --faint: #77777d;
      --line: #333338;
      --accent: #6ea0ff;
      --badge-bg: #2c2c30;
      --estimate-fg: #e0b64a;
    }
    .warnings {
      background: #2a2610;
      border-color: #4a421c;
      color: #d8c98a;
    }
    .badge.plugin {
      background: #22304d;
      color: #9dbcf5;
    }
    .badge.project {
      background: #1f3524;
      color: #8fd39c;
    }
    .state.error code {
      color: #ff9a90;
    }
  }
</style>
