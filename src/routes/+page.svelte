<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import {
    enable as autostartEnable,
    disable as autostartDisable,
    isEnabled as autostartIsEnabled,
  } from "@tauri-apps/plugin-autostart";
  import { onMount } from "svelte";
  import {
    coResidentAlwaysOn,
    DEFAULT_SORT,
    dependentsBadge,
    dependentsTitle,
    estimatedLayerCount,
    groupByPlugin,
    groupProjectsByRepo,
    layerDisplay,
    mainSkills,
    managerRootDisplay,
    managerRootTitle,
    normalizeApiKey,
    repoBasename,
    scannedPaths,
    skillKey,
    skillNameTitle,
    sortSkills,
    usageDisplay,
    usageTitle,
    type LayerReport,
    type ScanReport,
    type SetKeyOutcome,
    type SkillReport,
    type SortColumn,
    type SortState,
    type UsageSettings,
  } from "$lib/skills";

  let report = $state<ScanReport | null>(null);
  let error = $state<string | null>(null);
  let loading = $state(true);

  // Attributed-usage scope toggle (issue #13). Off by default: the headline
  // usage metric excludes sub-agent tokens. Flipping it re-scans with the
  // sub-agent transcripts folded in — a backend re-scan param, never a
  // frontend filter, since the tokens must come from the deduped store.
  let includeSubagents = $state(false);

  // API-key settings (issue #4). The panel is a view-swap: the gear replaces
  // the skill table with a settings pane, never a modal on this small surface.
  let view = $state<"table" | "settings">("table");
  let keyInput = $state("");
  let saving = $state(false);
  let setOutcome = $state<SetKeyOutcome | null>(null);
  let keyError = $state<string | null>(null);
  // True only while the first post-save rescan is running, so we can explain
  // the long count_tokens burst instead of the plain "Scanning…" (looks hung).
  let firstKeyScan = $state(false);

  // Rolling-window toggle (issue #14). The panel defaults to all-time (issue
  // #5's shipped cumulative figures); 24h is opt-in. The budget toast is always
  // evaluated on a 24h window regardless of this view.
  let windowHours = $state<number | null>(null);

  // Usage-toast settings (issue #14), loaded lazily when the settings pane opens.
  let usageSettings = $state<UsageSettings | null>(null);
  let savingUsage = $state(false);

  // Click-to-sort state (DESIGN.md UX #2). Purely client-side over the last
  // scan — changing the sort never re-invokes the backend. A shallow copy of the
  // default so re-sorting is a fresh object each time and Svelte tracks it.
  let sort = $state<SortState>({ ...DEFAULT_SORT });

  // Group-by-plugin toggle (DESIGN.md UX #2), opt-in. Clusters the main list
  // under plugin/personal headers; the flat list is the default.
  let groupByPluginOn = $state(false);

  // Launch-at-login (the autostart plugin), surfaced in the settings pane. The
  // checkbox mirrors the real OS state, loaded lazily when settings opens.
  let autostartOn = $state(false);
  let autostartLoading = $state(false);

  const apiKeyPresent = $derived(report?.apiKeyPresent ?? false);
  const trimmedKey = $derived(normalizeApiKey(keyInput));
  const estimatedLayers = $derived(report ? estimatedLayerCount(report) : 0);

  const allSkills = $derived(report?.skills ?? []);
  const activeRepoPath = $derived(report?.activeRepoPath ?? null);
  // The main list is personal + plugin skills (project skills live in their own
  // per-repo sections, DESIGN.md UX #5), sorted by the current column/direction.
  const mainRows = $derived(sortSkills(mainSkills(allSkills), sort));
  const pluginGroups = $derived(groupByPlugin(allSkills, sort));
  const repoSections = $derived(groupProjectsByRepo(allSkills, activeRepoPath, sort));
  // The global always-on total: personal + live plugins + the active repo's
  // project skills only (DESIGN.md UX #5). `~`-marked when any part is estimated.
  const alwaysOnTotal = $derived(coResidentAlwaysOn(allSkills, activeRepoPath));
  const hasSkills = $derived(allSkills.length > 0);
  const hasMain = $derived(mainRows.length > 0);

  async function load() {
    loading = true;
    error = null;
    try {
      report = await invoke<ScanReport>("list_skills", { includeSubagents, usageWindowHours: windowHours });
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  // Switch the displayed usage window and rescan. No-op if already selected.
  async function setWindow(hours: number | null) {
    if (windowHours === hours) return;
    windowHours = hours;
    await load();
  }

  // Toggle sort: same column flips direction; a new column starts descending
  // for numbers (heaviest first) and ascending for the name (A→Z).
  function onSort(column: SortColumn) {
    if (sort.column === column) {
      sort = { column, direction: sort.direction === "asc" ? "desc" : "asc" };
    } else {
      sort = { column, direction: column === "name" ? "asc" : "desc" };
    }
  }

  function ariaSort(column: SortColumn): "ascending" | "descending" | "none" {
    if (sort.column !== column) return "none";
    return sort.direction === "asc" ? "ascending" : "descending";
  }

  // Validate + store the key, then rescan so the exact/estimate badges flip.
  // A rejected key keeps the input and does not rescan; a stored key is
  // cleared from component state so the secret isn't retained (issue #4).
  async function saveKey() {
    if (!trimmedKey || saving) return;
    saving = true;
    keyError = null;
    setOutcome = null;
    try {
      const outcome = await invoke<SetKeyOutcome>("set_api_key", { key: keyInput });
      setOutcome = outcome;
      if (outcome !== "rejected") {
        keyInput = "";
        firstKeyScan = true;
        await load();
        firstKeyScan = false;
      }
    } catch (e) {
      keyError = String(e);
    } finally {
      saving = false;
    }
  }

  async function removeKey() {
    if (saving) return;
    saving = true;
    keyError = null;
    setOutcome = null;
    try {
      await invoke("delete_api_key");
      await load();
    } catch (e) {
      keyError = String(e);
    } finally {
      saving = false;
    }
  }

  async function loadUsageSettings() {
    try {
      usageSettings = await invoke<UsageSettings>("get_usage_settings");
    } catch (e) {
      keyError = String(e);
    }
  }

  async function saveUsageSettings() {
    if (!usageSettings || savingUsage) return;
    savingUsage = true;
    keyError = null;
    try {
      await invoke("set_usage_settings", { settings: usageSettings });
    } catch (e) {
      keyError = String(e);
    } finally {
      savingUsage = false;
    }
  }

  // Reflect the OS autostart state into the checkbox. Non-fatal on failure:
  // launch-at-login is a convenience, so a read error leaves the toggle off
  // rather than blocking the settings pane.
  async function loadAutostart() {
    try {
      autostartOn = await autostartIsEnabled();
    } catch (e) {
      keyError = String(e);
    }
  }

  async function toggleAutostart(next: boolean) {
    if (autostartLoading) return;
    autostartLoading = true;
    keyError = null;
    try {
      if (next) await autostartEnable();
      else await autostartDisable();
      autostartOn = await autostartIsEnabled();
    } catch (e) {
      keyError = String(e);
      // Re-sync to the real state so the checkbox never lies about what the OS did.
      await loadAutostart();
    } finally {
      autostartLoading = false;
    }
  }

  function openSettings() {
    setOutcome = null;
    keyError = null;
    view = "settings";
    loadUsageSettings();
    loadAutostart();
  }

  function closeSettings() {
    view = "table";
  }

  // Each footprint cell carries a tooltip stating whether the number is exact
  // or a calibrated estimate, so the two tiers are never conflated (ADR 0003/0006).
  function layerTitle(layer: LayerReport): string {
    return layer.exact ? "Exact count" : "Calibrated tiktoken estimate, not exact";
  }

  function alwaysOnTitle(skill: SkillReport): string {
    // A never-listed skill is not an imprecise count but the absence of one, so
    // it never inherits the exact/estimate framing (issue #24).
    if (skill.alwaysOnText === "notListed") {
      return "Not in the skill listing (disable-model-invocation), so it costs no always-on tokens. Still invokable as a slash command";
    }
    const base = layerTitle(skill.alwaysOn);
    return skill.alwaysOnText === "native"
      ? base
      : `${base}. Always-on text reconstructed from frontmatter; no session has listed this skill yet`;
  }

  function onDemandTitle(layer: LayerReport | null): string {
    if (layer === null) return "Computing on-demand ceiling…";
    const base = "Ceiling: raw size of bundled references, loaded only if the body reads them";
    return layer.exact ? base : `${base} (calibrated estimate)`;
  }

  onMount(() => {
    load();
    // The registry watcher (ADR 0019) fires this when a skill/plugin surface
    // changes; re-scan so the list doesn't go stale. Enablement is read at
    // session start, so this is a freshness nudge, not a live-state mirror.
    const unlistenRegistry = listen("registry-changed", () => load());
    // The background on-demand fill (issue #11) fires this once it has computed
    // the pending ceilings; re-scan so the "…" cells resolve to real numbers.
    const unlistenOnDemand = listen("on-demand-ready", () => load());
    return () => {
      unlistenRegistry.then((off) => off());
      unlistenOnDemand.then((off) => off());
    };
  });
</script>

{#snippet layerCell(layer: LayerReport, title: string, reconstructed = false)}
  <div class="col num" role="cell" class:estimate={!layer.exact} class:reconstructed title={title}>
    {layerDisplay(layer)}
  </div>
{/snippet}

{#snippet numHeader(column: SortColumn, label: string)}
  <div class="col num colhead" role="columnheader" aria-sort={ariaSort(column)}>
    <button class="sort-btn" class:sorted={sort.column === column} onclick={() => onSort(column)}>
      {#if sort.column === column}<span class="ind">{sort.direction === "desc" ? "▼" : "▲"}</span>{/if}{label}
    </button>
  </div>
{/snippet}

{#snippet tableHeader()}
  <div class="row header" role="row">
    <div class="col name colhead" role="columnheader" aria-sort={ariaSort("name")}>
      <button class="sort-btn" class:sorted={sort.column === "name"} onclick={() => onSort("name")}>
        Skill{#if sort.column === "name"}<span class="ind">{sort.direction === "desc" ? "▼" : "▲"}</span>{/if}
      </button>
    </div>
    {@render numHeader("alwaysOn", "Always-on")}
    {@render numHeader("onInvoke", "On-invoke")}
    {@render numHeader("onDemand", "On-demand")}
  </div>
{/snippet}

{#snippet skillRow(skill: SkillReport, inRepoSection = false)}
  {@const dependents = dependentsBadge(skill.providesFor)}
  <!-- A managed skill renders as two rows (itself, then its manager root), and
       `rowgroup` is what ties them together: it keeps the path from reading as a
       standalone row to a screen reader, and gives hover one element to light up
       instead of highlighting half a skill. -->
  <div class="skill-group" role="rowgroup" class:inactive={!skill.live}>
    <div class="row" role="row" class:has-manager={!!skill.managerRoot}>
      <div class="col name" role="cell">
        <div class="name-line">
          <span class="skill-name" title={skillNameTitle(skill)}>{skill.id.name}</span>
          {#if skill.id.kind === "plugin"}
            <span class="badge plugin" title="Plugin-locked: remove the whole plugin, not one skill">
              {skill.id.plugin}
            </span>
          {:else if skill.id.kind === "project" && !inRepoSection}
            <span class="badge project" title={skill.id.repoPath}>{repoBasename(skill.id.repoPath)}</span>
          {/if}
          {#if dependents}
            <!-- Never demoted to the manager root's line: this is the fact that
                 inverts the row's meaning. Unmanaged reads as "safe to delete"
                 on exactly the row that takes 46 skills with it. -->
            <span class="badge dependents" title={dependentsTitle(skill.providesFor)}>{dependents}</span>
          {/if}
          {#if !skill.live}
            <span class="badge inactive-badge" title="Not live in the active context (contributes zero live footprint)">inactive</span>
          {/if}
        </div>
        {#if skill.usage}
          <div class="usage" title={usageTitle(skill.usage)}>
            {usageDisplay(skill.usage, report?.usageWindowHours ?? null)}
          </div>
        {/if}
      </div>

      {#if skill.alwaysOnText === "notListed"}
        <!-- A real 0, not the "…" a pending ceiling gets nor an em dash: this
             cost is known to be nothing, not unknown (issue #24). -->
        <div class="col num not-listed" role="cell" title={alwaysOnTitle(skill)}>0</div>
      {:else}
        {@render layerCell(skill.alwaysOn, alwaysOnTitle(skill), skill.alwaysOnText === "reconstructed")}
      {/if}
      {@render layerCell(skill.onInvoke, layerTitle(skill.onInvoke))}
      {#if skill.onDemand === null}
        <div class="col num pending" role="cell" title={onDemandTitle(null)}>…</div>
      {:else}
        {@render layerCell(skill.onDemand, onDemandTitle(skill.onDemand))}
      {/if}
    </div>

    {#if skill.managerRoot}
      <!-- The path, elided but never renamed: no basename rule survives a real
           machine (ADR 0026). A row of its own, rather than a fifth cell or a
           line inside the name cell: a real manager root is a deep path, the
           name column cannot hold one without eliding away the very segment that
           names the manager, and a fifth cell would make the row ragged against
           a four-column header. "in" because a bare path under a row named
           `ship` would read as `ship`'s own directory, which it is not. -->
      <div class="manager-row" role="row">
        <span class="manager" role="cell" title={managerRootTitle(skill.managerRoot)}>
          in {managerRootDisplay(skill.managerRoot)}
        </span>
      </div>
    {/if}
  </div>
{/snippet}

<main>
  {#if view === "settings"}
    <header class="topbar">
      <button class="icon-btn back" onclick={closeSettings} aria-label="Back to skills" title="Back">‹</button>
      <h1>Settings</h1>
      <span class="spacer"></span>
    </header>

    <section class="settings">
      <h2 class="settings-heading">API key</h2>
      {#if apiKeyPresent}
        <p class="set-status">API key set.</p>
        <button class="danger" onclick={removeKey} disabled={saving}>
          {saving ? "Removing…" : "Remove key"}
        </button>
        <p class="hint">
          Removing the key stops new exact counts. Counts already computed stay exact until their skill changes.
        </p>
      {:else}
        <p class="hint">
          Paste an Anthropic Console API key to get exact token counts instead of estimates. It's stored in your
          operating system's keychain and used only to call Anthropic's count_tokens endpoint.
        </p>
        <label class="field">
          <span class="field-label">Console API key</span>
          <input
            type="password"
            autocomplete="off"
            spellcheck="false"
            placeholder="sk-ant-..."
            bind:value={keyInput}
            disabled={saving}
            onkeydown={(e) => e.key === "Enter" && saveKey()}
          />
        </label>
        <button class="primary" onclick={saveKey} disabled={!trimmedKey || saving}>
          {saving ? "Saving…" : "Save key"}
        </button>
        <p class="hint why">
          Claude Code's own login can't be reused for this, so exact counts need your own key from console.anthropic.com.
        </p>
      {/if}

      {#if setOutcome === "rejected"}
        <p class="notice rejected">
          Anthropic rejected that key. Check you pasted a Console API key from console.anthropic.com, not your Claude
          Code login.
        </p>
      {:else if setOutcome === "storedUnverified"}
        <p class="notice warn">
          Key saved, but skillmon couldn't reach Anthropic to verify it. Counts turn exact once it can.
        </p>
      {:else if setOutcome === "stored"}
        <p class="notice ok">Key saved. Counting exact footprints now.</p>
      {/if}
      {#if keyError}
        <p class="notice rejected">Something went wrong. <code>{keyError}</code></p>
      {/if}
    </section>

    {#if usageSettings}
      <section class="settings usage-settings">
        <h2 class="settings-heading">Usage toasts</h2>
        <label class="check">
          <input type="checkbox" bind:checked={usageSettings.budgetEnabled} disabled={savingUsage} />
          <span>Warn when 24h attributed work goes over a budget</span>
        </label>
        <label class="field indented">
          <span class="field-label">Budget (work tokens per 24h)</span>
          <input
            type="number"
            min="0"
            step="1000"
            bind:value={usageSettings.budgetWorkTokens}
            disabled={savingUsage || !usageSettings.budgetEnabled}
          />
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={usageSettings.anomalyEnabled} disabled={savingUsage} />
          <span>Also warn when one skill spikes above its usual daily average</span>
        </label>
        <button class="primary" onclick={saveUsageSettings} disabled={savingUsage}>
          {savingUsage ? "Saving…" : "Save usage settings"}
        </button>
        <p class="hint why">
          An estimate of tokens spent while skills were active, not a bill. The check runs each time the panel opens,
          not in real time.
        </p>
      </section>
    {/if}

    <section class="settings startup-settings">
      <h2 class="settings-heading">Startup</h2>
      <label class="check">
        <input
          type="checkbox"
          checked={autostartOn}
          disabled={autostartLoading}
          onchange={(e) => toggleAutostart(e.currentTarget.checked)}
        />
        <span>Launch skillmon at login</span>
      </label>
      <p class="hint why">
        Toggle the panel from anywhere with <kbd>⌘⇧K</kbd>.
      </p>
    </section>
  {:else}
    <header class="topbar">
      <h1>Skills</h1>
      <div class="topbar-right">
        <button class="rescan" onclick={load} disabled={loading} title="Rescan now">
          {loading ? "Scanning…" : "Rescan"}
        </button>
        <button class="icon-btn" onclick={openSettings} aria-label="Settings" title="Settings">
          <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <circle cx="12" cy="12" r="3"></circle>
            <path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"></path>
          </svg>
        </button>
      </div>
    </header>

    <div class="controls">
      <div class="window-toggle" role="group" aria-label="Usage window">
        <button
          class="seg"
          class:active={windowHours === null}
          onclick={() => setWindow(null)}
          disabled={loading}
          title="Show all-time attributed usage"
        >All-time</button>
        <button
          class="seg"
          class:active={windowHours === 24}
          onclick={() => setWindow(24)}
          disabled={loading}
          title="Show attributed usage from the last 24 hours"
        >Last 24h</button>
      </div>
      <label
        class="inline-toggle"
        title="Include sub-agent usage. Only sub-agents that themselves invoked a skill are credited; the rest are dropped."
      >
        <input
          type="checkbox"
          checked={includeSubagents}
          disabled={loading}
          onchange={(e) => {
            includeSubagents = e.currentTarget.checked;
            load();
          }}
        />
        Sub-agents
      </label>
      <label class="inline-toggle" title="Group skills under their plugin; personal skills grouped together">
        <input type="checkbox" bind:checked={groupByPluginOn} />
        Group by plugin
      </label>
      <span class="controls-spacer"></span>
      {#if activeRepoPath}
        <span class="active-repo" title={activeRepoPath}>active: {repoBasename(activeRepoPath)}</span>
      {/if}
    </div>

    {#if report?.warnings?.length}
      <ul class="warnings">
        {#each report.warnings as warning}
          <li>{warning}</li>
        {/each}
      </ul>
    {/if}

    {#if firstKeyScan}
      <div class="banner info">
        Counting exact footprints for the first time. This can take a while on a large skill set.
      </div>
    {:else if apiKeyPresent && estimatedLayers > 0}
      <div class="banner soft">
        Some counts couldn't be fetched exactly this scan.
        <button class="linklike" onclick={load} disabled={loading}>Rescan to retry.</button>
      </div>
    {/if}

    {#if error}
      <div class="state error">
        <p>Couldn't load skills.</p>
        <code>{error}</code>
        <button onclick={load}>Try again</button>
      </div>
    {:else if loading && !report}
      <div class="state muted">Scanning skills…</div>
    {:else if !hasSkills}
      <div class="state muted empty">
        <p>No skills found. skillmon scanned:</p>
        <ul class="scanned-paths">
          {#each scannedPaths(activeRepoPath) as path (path)}
            <li><code>{path}</code></li>
          {/each}
        </ul>
        <button onclick={load}>Rescan</button>
      </div>
    {:else}
      {#if hasMain}
        <div class="table" role="table" aria-label="Installed skills">
          {@render tableHeader()}
          {#if groupByPluginOn}
            {#each pluginGroups as group (group.key)}
              <div class="group-label" role="row"><span role="cell">{group.label}</span></div>
              {#each group.skills as skill (skillKey(skill))}
                {@render skillRow(skill)}
              {/each}
            {/each}
          {:else}
            {#each mainRows as skill (skillKey(skill))}
              {@render skillRow(skill)}
            {/each}
          {/if}
        </div>
      {/if}

      {#if repoSections.length}
        <section class="repo-sections" aria-label="Project skills by repo">
          {#each repoSections as repo (repo.repoPath)}
            <details class="repo-section" open={repo.isActive}>
              <summary>
                <span class="repo-summary-name" title={repo.repoPath}>{repo.repoName}</span>
                <span class="repo-count">{repo.skills.length} {repo.skills.length === 1 ? "skill" : "skills"}</span>
                {#if repo.isActive}<span class="badge project">active</span>{/if}
              </summary>
              <div class="table" role="table" aria-label={`Project skills in ${repo.repoName}`}>
                {#each repo.skills as skill (skillKey(skill))}
                  {@render skillRow(skill, true)}
                {/each}
              </div>
            </details>
          {/each}
        </section>
      {/if}

      <footer class="legend">
        <span
          class="total"
          title="Always-on tokens co-resident now: personal + live plugins + the active repo's project skills (DESIGN #5). Other repos are shown but not summed."
        >
          Always-on now: <strong>{layerDisplay(alwaysOnTotal)}</strong>
        </span>
        {#if apiKeyPresent}
          <span><span class="swatch estimate">~</span> calibrated estimate</span>
        {:else}
          <button class="linklike swatch-link" onclick={openSettings}>
            <span class="swatch estimate">~</span> calibrated estimate. Add an API key for exact counts
          </button>
        {/if}
        <span>On-demand is a ceiling.</span>
      </footer>
    {/if}
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

  /* Controls bar under the title — the toggles wrap here so the narrow panel
     never overflows its topbar. */
  .controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    padding: 6px 12px;
    border-bottom: 1px solid var(--line);
  }
  .controls-spacer {
    flex: 1 1 auto;
  }

  .active-repo {
    color: var(--muted);
    font-size: 11px;
    max-width: 150px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* Segmented All-time / Last 24h control (issue #14). */
  .window-toggle {
    display: inline-flex;
    border: 1px solid var(--line);
    border-radius: 6px;
    overflow: hidden;
  }
  .window-toggle .seg {
    border: none;
    border-radius: 0;
    padding: 3px 8px;
    font-size: 11px;
    color: var(--muted);
  }
  .window-toggle .seg + .seg {
    border-left: 1px solid var(--line);
  }
  .window-toggle .seg.active {
    background: var(--accent);
    color: #fff;
  }
  .window-toggle .seg.active:hover:not(:disabled) {
    color: #fff;
  }

  /* Demoted, muted inline checkbox controls (sub-agents scope, group-by-plugin):
     they sit in the controls bar, matching the demoted framing of what they change. */
  .inline-toggle {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--muted);
    font-size: 11px;
    white-space: nowrap;
    cursor: pointer;
  }
  .inline-toggle input {
    margin: 0;
    cursor: pointer;
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

  /* Empty-state: the exact paths a scan looked at (DESIGN.md UX #7). */
  .scanned-paths {
    list-style: none;
    margin: 8px auto;
    padding: 0;
    display: inline-block;
    text-align: left;
  }
  .scanned-paths li {
    font-size: 11px;
    margin: 2px 0;
  }
  .scanned-paths code {
    color: var(--muted);
    word-break: break-all;
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
    z-index: 1;
  }
  /* Hover and liveness belong to the skill, not to one of the rows it renders
     as: a managed skill is a row plus its manager-root row, and highlighting or
     dimming half of one would read as two unrelated things. */
  .skill-group:hover {
    background: rgba(57, 108, 216, 0.06);
  }
  .skill-group.inactive {
    opacity: 0.55;
  }

  /* A plugin/personal cluster label in the grouped view (DESIGN.md UX #2). */
  .group-label {
    padding: 8px 12px 3px;
    font-size: 10px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--muted);
    border-bottom: 1px solid var(--line);
  }

  .col {
    min-width: 0;
  }
  .col.name {
    display: flex;
    flex-direction: column;
    justify-content: center;
    align-items: flex-start;
    gap: 2px;
    overflow: hidden;
  }
  .name-line {
    display: flex;
    align-items: center;
    gap: 6px;
    overflow: hidden;
    max-width: 100%;
  }
  .skill-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 500;
    /* Never let a row's badges squeeze the name to nothing on the narrow panel:
       the name keeps a legible floor and the badges shrink/ellipsize first. */
    flex: 0 1 auto;
    min-width: 2.75rem;
  }
  /* The demoted sub-lines under a row.

     `.usage` is attributed usage: a proxy, never a headline column and never
     blended with the exact footprint (ADR 0003). `.manager` is the manager
     root: who restores this row, which matters when removing it, not while
     reading the footprint. Both are clipped as a backstop only: the ellipsis
     takes the tail, and for a path the tail is the half worth reading, so
     `managerRootDisplay` elides from the left before it ever gets here. */
  .usage,
  .manager {
    font-size: 10px;
    color: var(--faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 100%;
  }
  /* The manager root's continuation row: the path gets the whole row's width
     instead of the name column's ~112px. The width IS the design: at this size
     a real manager root fits whole, and nothing has to guess which of its
     segments identifies the manager. It carries the border its row gave up, so
     the two read as one row and no rule is drawn between a skill and its path. */
  .manager-row {
    padding: 0 12px 5px;
    border-bottom: 1px solid var(--line);
  }
  /* The border moves to the manager row, so no rule is drawn between a skill and
     its own path. Gated on the same condition that renders that row, so the two
     cannot disagree. */
  .row.has-manager {
    border-bottom: none;
  }
  .manager {
    display: block;
  }
  .col.num {
    text-align: right;
    font-variant-numeric: tabular-nums;
    font-feature-settings: "tnum";
    white-space: nowrap;
  }

  /* Sortable column headers: the whole header cell is a button (DESIGN.md UX #2). */
  .colhead {
    padding: 0;
  }
  .sort-btn {
    border: none;
    background: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: inherit;
    cursor: pointer;
    width: 100%;
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .col.name .sort-btn {
    justify-content: flex-start;
  }
  .col.num .sort-btn {
    justify-content: flex-end;
  }
  .sort-btn:hover:not(:disabled) {
    color: var(--accent);
  }
  .sort-btn.sorted {
    color: var(--fg);
  }
  .ind {
    font-size: 9px;
    color: var(--accent);
  }

  .badge {
    /* Shrinkable (not flex:none) so a badge ellipsizes before it can starve the
       skill name of width; a small floor keeps it from vanishing entirely. */
    flex: 0 1 auto;
    min-width: 1.75rem;
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
  /* The one badge that is a warning: removing this row removes everything that
     resolves into it (ADR 0027). Rare by nature, since it marks a managing
     tool's own entry, 1 row of 71 on a real machine, so the colour costs
     nothing and earns the glance. */
  .badge.dependents {
    background: #fbeee8;
    color: #a4501c;
  }

  /* An estimate is muted and marked; it never blends with an exact count. */
  .col.num.estimate {
    color: var(--estimate-fg);
  }
  /* On-demand ceiling still being computed off the interactive scan (issue
     #11): a faint ellipsis, never a 0 that would read as a resolved ceiling. */
  .col.num.pending {
    color: var(--faint);
  }
  /* Always-on text reconstructed from frontmatter (no transcript yet). */
  .col.num.reconstructed {
    text-decoration: underline dotted;
    text-underline-offset: 3px;
  }
  /* Never listed to the model, so there is no always-on line to count (issue
     #24). Muted to set it apart from a counted figure, but it carries neither
     the estimate nor the reconstructed mark: the zero is certain. */
  .col.num.not-listed {
    color: var(--faint);
  }

  /* Per-repo collapsed project sections (DESIGN.md UX #5). */
  .repo-sections {
    display: flex;
    flex-direction: column;
  }
  .repo-section {
    border-bottom: 1px solid var(--line);
  }
  .repo-section > summary {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 7px 12px;
    cursor: pointer;
    font-size: 11px;
    color: var(--muted);
    list-style: none;
    user-select: none;
  }
  .repo-section > summary::-webkit-details-marker {
    display: none;
  }
  .repo-section > summary::before {
    content: "▸";
    font-size: 9px;
    color: var(--faint);
  }
  .repo-section[open] > summary::before {
    content: "▾";
  }
  .repo-summary-name {
    font-weight: 600;
    color: var(--fg);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .repo-count {
    color: var(--faint);
    flex: none;
  }

  .legend {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    align-items: baseline;
    padding: 8px 12px 10px;
    color: var(--faint);
    font-size: 10px;
  }
  .total {
    color: var(--muted);
    font-size: 11px;
  }
  .total strong {
    color: var(--fg);
    font-variant-numeric: tabular-nums;
  }
  .swatch.estimate {
    color: var(--estimate-fg);
    font-weight: 600;
  }

  /* Icon buttons: the settings gear and the back arrow. */
  .icon-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 3px 6px;
    line-height: 1;
    color: var(--muted);
  }
  .icon-btn:hover:not(:disabled) {
    color: var(--accent);
    border-color: var(--accent);
  }
  .icon-btn.back {
    font-size: 18px;
    padding: 0 8px;
  }
  .spacer {
    width: 28px;
  }

  /* Settings pane (the gear's view-swap). */
  .settings {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 12px;
    max-width: 420px;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .field-label {
    font-size: 11px;
    color: var(--muted);
  }
  .settings input {
    font-family: inherit;
    font-size: 12px;
    color: var(--fg);
    background: #fff;
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 5px 8px;
  }
  .settings input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .settings button.primary {
    align-self: flex-start;
    color: #fff;
    background: var(--accent);
    border-color: var(--accent);
  }
  .settings button.primary:disabled {
    opacity: 0.5;
    color: #fff;
  }
  .settings button.danger {
    align-self: flex-start;
    color: #b3261e;
    border-color: #e6b4b0;
  }
  .settings button.danger:hover:not(:disabled) {
    color: #b3261e;
    border-color: #b3261e;
  }
  .usage-settings,
  .startup-settings {
    border-top: 1px solid var(--line);
    padding-top: 12px;
  }
  .settings-heading {
    font-size: 12px;
    font-weight: 600;
    margin: 0;
  }
  .check {
    display: flex;
    align-items: flex-start;
    gap: 6px;
    font-size: 11px;
    color: var(--fg);
    cursor: pointer;
  }
  .check input {
    margin-top: 1px;
  }
  .field.indented {
    margin-left: 22px;
  }
  /* The number input inherits base styling (incl. dark mode) from `.settings
     input`; it only needs its own width. */
  .usage-settings input[type="number"] {
    max-width: 160px;
  }
  kbd {
    font-family: inherit;
    font-size: 11px;
    background: var(--badge-bg);
    border: 1px solid var(--line);
    border-radius: 4px;
    padding: 0 4px;
  }
  .set-status {
    font-weight: 500;
    margin: 0;
  }
  .hint {
    margin: 0;
    color: var(--muted);
    font-size: 11px;
    line-height: 1.45;
  }
  .hint.why {
    color: var(--faint);
  }

  /* Inline outcome messages in the settings pane. */
  .notice {
    margin: 2px 0 0;
    padding: 6px 10px;
    border-radius: 6px;
    font-size: 11px;
    line-height: 1.4;
  }
  .notice.ok {
    background: #eaf6ec;
    color: #2f7d3a;
  }
  .notice.warn {
    background: #fff8e6;
    color: #6b5900;
  }
  .notice.rejected {
    background: #fdeceb;
    color: #b3261e;
  }
  .notice code {
    word-break: break-word;
  }

  /* Table-view banners: first-key progress and the fall-back retry nudge. */
  .banner {
    margin: 8px 12px 0;
    padding: 6px 10px;
    border-radius: 6px;
    font-size: 11px;
    line-height: 1.4;
  }
  .banner.info {
    background: #e8effb;
    color: #2f5bb7;
  }
  .banner.soft {
    background: var(--badge-bg);
    color: var(--muted);
  }

  /* A button styled as inline text (legend CTA, "rescan to retry"). */
  .linklike {
    border: none;
    background: none;
    padding: 0;
    font: inherit;
    color: var(--accent);
    cursor: pointer;
  }
  .linklike:hover:not(:disabled) {
    text-decoration: underline;
  }
  .linklike:disabled {
    color: var(--faint);
    cursor: default;
  }
  .legend .swatch-link {
    font-size: 10px;
    color: var(--faint);
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }
  .legend .swatch-link:hover {
    color: var(--accent);
    text-decoration: none;
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
    .badge.dependents {
      background: #40281c;
      color: #e8a677;
    }
    .state.error code {
      color: #ff9a90;
    }
    .settings input {
      background: #2a2a2e;
    }
    .notice.ok {
      background: #1f3524;
      color: #8fd39c;
    }
    .notice.warn {
      background: #2a2610;
      color: #d8c98a;
    }
    .notice.rejected {
      background: #3a1f1d;
      color: #ff9a90;
    }
    .banner.info {
      background: #22304d;
      color: #9dbcf5;
    }
    .settings button.danger {
      color: #ff9a90;
      border-color: #5a3a37;
    }
  }
</style>
