// skillmon — Tauri core entry point.
// Menu-bar (macOS) / system-tray (Windows) app. Domain language: ../CONTEXT.md.
// Architecture and decisions: ../../docs/DESIGN.md, ../../docs/adr/.

mod adapters;
mod domain;
mod footprint;
mod managing_tool;
mod removal;
mod self_write;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use adapters::claude_code::listing_cache::SqliteListingCache;
use adapters::claude_code::on_demand_cache::SqliteOnDemandCache;
use adapters::claude_code::paths::default_claude_home;
use adapters::claude_code::usage_cache::SqliteUsageCache;
use adapters::claude_code::watcher::RegistryWatcher;
use adapters::claude_code::{fill_on_demand_ceilings, ClaudeCodeAdapter, REFERENCE_MODEL_ID};
use domain::removal::TrashUnitId;
use domain::report::{PurgeSummary, ScanReport, TombstoneReport, TrashUnitReport, UsageSettings};
use domain::scan::{ScanParams, UsageWindow};
use domain::skill::SkillId;
use footprint::api_key_service::{self, SetKeyOutcome};
use footprint::api_key_store::{ApiKeyStore, KeychainApiKeyStore};
use footprint::cache::TokenCache;
use footprint::count_tokens_client::{AnthropicCountTokensClient, CountTokensClient};
use footprint::tokenizer::BpeTokenizer;
use managing_tool::ManagingTools;
use removal::store::TrashStore;
use self_write::SelfWriteWindow;

/// The scan orchestration is synchronous (ADR 0008) and can block on file
/// I/O or a `count_tokens` call, so it lives behind a `Mutex` and is only
/// ever touched from the blocking pool. `Arc<Mutex<_>>` is the `Send + Sync`
/// shape Tauri's managed state requires; `TokenCache`'s `rusqlite`
/// connection is `Send` but not `Sync`, which is exactly why the `Mutex` is
/// mandatory, not merely convenient.
type SharedAdapter = Arc<Mutex<ClaudeCodeAdapter>>;
type SharedWatcher = Arc<Mutex<RegistryWatcher>>;

/// The API-key settings surface (issue #4), managed SEPARATELY from the
/// adapter on purpose. A keychain write can block on a modal OS auth dialog
/// for an unbounded time, and a first-key scan can run for many seconds, so
/// sharing the adapter's scan `Mutex` would let a Save freeze behind a scan
/// and a scan freeze the whole UI behind a keychain prompt. The store here is
/// a second stateless handle over the same keychain entry the adapter reads
/// (same SERVICE/USERNAME), so the two stay coherent; the client is used only
/// to validate a key on save.
struct ApiKeySettings {
    store: Box<dyn ApiKeyStore>,
    client: Box<dyn CountTokensClient>,
}
type SharedApiKeySettings = Arc<Mutex<ApiKeySettings>>;

/// The trash ledger (issue #28, ADR 0029), managed SEPARATELY from the adapter
/// for the same reason `ApiKeySettings` is. A purge deletes real trees -- 1.1 GB
/// for a gstack uninstall -- and blocks for as long as that takes, so sharing
/// the adapter's scan `Mutex` would freeze the whole panel behind an
/// `Empty trash`. Its `rusqlite` connection is `Send` but not `Sync`, so the
/// `Mutex` is mandatory rather than merely convenient.
type SharedTrashStore = Arc<Mutex<TrashStore>>;

/// The managing tools skillmon knows how to ask (ADR 0027), resolved once at the
/// composition root because `.agents` reads `XDG_STATE_HOME` to find its own lock.
///
/// No `Mutex`, unlike every other piece of state here: the tools hold paths and
/// nothing else, and each call reads and writes the tool's file within it. So
/// there is no shared mutable state to guard, and wrapping it would only queue a
/// removal behind an unrelated one.
type SharedManagingTools = Arc<ManagingTools>;

// TODO(skillmon): the mutations that CREATE trash units (disable/enable,
// uninstall, remove_plugin) land with issue #31. This file wires only the
// reversal side (ADR 0029): list, restore, purge.
//
// Each of those must hold a `SelfWriteWindow` guard across its writes and emit
// `registry-changed` itself when its ledger write settles, exactly as
// `restore_trash_unit` does -- they write inside the recursively watched scan
// root, and suppression is not automatic (issue #29, ADR 0019 Update 4).
// `purge`/`empty_trash` deliberately do neither: they only ever touch
// `skillmon/removed/`, which no watcher watches (`paths::removed_dir`).

/// Discover every skill and compute its three-layer footprint (ADR 0019's
/// scan). The synchronous core runs on the blocking pool via
/// `spawn_blocking` (footprint-counter plan decision #4) so this async
/// command never stalls the runtime on I/O or a network call. Each scan also
/// re-syncs the registry watcher, so a repo that appeared since the last
/// scan starts being watched without waiting for a restart.
///
/// `usage_window_hours` picks which slice the per-skill usage figures cover:
/// `None` = all-time (the default view, issue #5's cumulative numbers),
/// `Some(24)` = the last 24h (issue #14). The 24h budget/anomaly toasts are
/// evaluated independently of this, always on a fixed 24h window. "Now" is
/// captured here at the command boundary, so the pure core holds no wall-clock.
///
/// `include_subagents` (issue #13) widens the attributed-usage pass to fold in
/// sub-agent transcripts. Tauri v2 maps the JS keys `includeSubagents` /
/// `usageWindowHours` to these snake_case parameters, so the frontend must
/// invoke with those exact camelCase keys or the command errors on a missing
/// argument.
#[tauri::command]
async fn list_skills(
    include_subagents: bool,
    usage_window_hours: Option<u32>,
    app: tauri::AppHandle,
    adapter: tauri::State<'_, SharedAdapter>,
    watcher: tauri::State<'_, SharedWatcher>,
    trash: tauri::State<'_, SharedTrashStore>,
) -> Result<ScanReport, String> {
    let adapter = adapter.inner().clone();
    let watcher = watcher.inner().clone();
    let now_millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let window = match usage_window_hours {
        Some(hours) => UsageWindow::Rolling { hours },
        None => UsageWindow::AllTime,
    };
    // Keep a handle for the issue #11 background fill below; the scan closure
    // consumes its own clone.
    let fill_adapter = adapter.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let adapter = adapter.lock().expect("adapter mutex poisoned");
        let outcome = adapter.scan(&ScanParams { now_millis, usage_window: window, include_subagents });
        adapter.sync_watcher(&mut watcher.lock().expect("watcher mutex poisoned"));
        outcome
    })
    .await
    .map_err(|e| e.to_string())?;

    // Emit toasts OUTSIDE the scan lock, after the core persisted the debounce
    // state (issue #14, D6): a failed show() is a lost nudge, never a stuck flag
    // that suppresses the next real crossing. A show() can fail if the OS
    // notification surface is unavailable (a headless Linux daemon, or Windows
    // without a registered AppUserModelID); log it and move on, since the panel
    // already renders the number.
    for toast in &outcome.toasts {
        let copy = toast.copy();
        if let Err(e) = app.notification().builder().title(copy.title).body(copy.body).show() {
            eprintln!("[skillmon] usage toast failed to show: {e}");
        }
    }

    // Issue #11: the interactive scan defers the on-demand ceiling, so any
    // skill whose ceiling is still pending gets filled off this response, on a
    // detached blocking task with its OWN sqlite connections. The task holds
    // the scan `Mutex` only for the cheap `pending_on_demand()` worklist, then
    // tokenizes off the lock so it neither blocks the next scan nor -- if a
    // background sqlite call panics -- poisons the adapter `Mutex`.
    if outcome.report.skills.iter().any(|s| s.on_demand.is_none()) {
        spawn_on_demand_fill(app, fill_adapter);
    }

    reconcile_tombstones(&outcome.report, trash.inner());

    Ok(outcome.report)
}

/// DESIGN.md UX #6's "reinstalling restores continuity", applied off every scan:
/// a skill that is discoverable again is not removed, whatever the ledger last
/// recorded (ADR 0029).
///
/// Driven by rediscovery rather than by restore alone because it has to cover
/// the case the ledger cannot see -- a user reinstalling by hand, past skillmon
/// entirely. Usage history is not touched here and never was: it is keyed by
/// `message.id` and outlives any removal (ADR 0024), so continuity is the
/// absence of a deletion rather than a recovery.
///
/// `try_lock`, deliberately: an `Empty trash` can hold this store for as long as
/// deleting 1.1 GB takes, and blocking a scan behind it would freeze the panel
/// to reconcile bookkeeping nothing is waiting on. Skipping is free -- the pass
/// is idempotent and the next scan runs it again.
fn reconcile_tombstones(report: &ScanReport, trash: &SharedTrashStore) {
    let Ok(mut store) = trash.try_lock() else { return };
    let discovered: Vec<SkillId> = report.skills.iter().map(|s| SkillId::from(s.id.clone())).collect();
    if let Err(e) = store.reconcile_tombstones(&discovered) {
        eprintln!("[skillmon] could not clear tombstones for rediscovered skills: {e}");
    }
}

/// Every staged removal, for the removed view (ADR 0029).
///
/// Includes `disabled` units as well as `trashed` ones: they are the same
/// operation with different retention (ADR 0027), and the panel decides which
/// affordances each label earns.
#[tauri::command]
async fn list_trash(trash: tauri::State<'_, SharedTrashStore>) -> Result<Vec<TrashUnitReport>, String> {
    let trash = trash.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = trash.lock().expect("trash mutex poisoned");
        store.list().map(|units| units.iter().map(TrashUnitReport::from).collect()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Undoes one staged removal, putting all of its entries back -- 47 or one (ADR
/// 0027's single undo for a tool uninstall).
///
/// Restored entries land back inside the recursively watched scan root, so this
/// suppresses the ADR 0019 watcher across the moves and then announces the
/// change itself (issue #29). The watcher would otherwise raise
/// `registry-changed` *while* the 47 moves are still running and the ledger has
/// not settled -- a rescan of a half-restored tree, several times over. Doing it
/// this way turns that into one rescan of a finished one.
///
/// The guard brackets the writes only, and is taken *after* the store lock
/// rather than before. Taking it first would look more cautious and be strictly
/// worse: the wait for that lock is unbounded (an `Empty trash` can hold it for
/// as long as deleting 1.1 GB takes), and every second of it would be a second
/// of the watcher ignoring changes while this command writes nothing at all. A
/// mutation already writing has its own guard; it does not need this one's.
#[tauri::command]
async fn restore_trash_unit(
    unit_id: i64,
    app: tauri::AppHandle,
    trash: tauri::State<'_, SharedTrashStore>,
    tools: tauri::State<'_, SharedManagingTools>,
    self_writes: tauri::State<'_, SelfWriteWindow>,
) -> Result<(), String> {
    let trash = trash.inner().clone();
    let tools = tools.inner().clone();
    let self_writes = self_writes.inner().clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let mut store = trash.lock().expect("trash mutex poisoned");
        let _writing = self_writes.open();
        removal::restore(&mut store, &tools, TrashUnitId(unit_id)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?;

    // Emitted whatever the outcome. A restore that fails *after* moving
    // something rolls back best-effort (`move_back` logs rather than returns),
    // so a failed restore is precisely the case where the tree can differ from
    // what the panel shows -- and the watcher was told not to mention it.
    // Announcing only successes would leave exactly that state invisible. The
    // price is a redundant rescan when the precheck refused and nothing moved,
    // which is the same work a panel reopen does anyway.
    //
    // A failure to emit is logged, not discarded: it is the one thing that
    // leaves the panel stale with nothing else due to correct it.
    if let Err(e) = app.emit("registry-changed", ()) {
        eprintln!("[skillmon] restore could not announce registry-changed; the panel may be stale: {e}");
    }
    outcome
}

/// Every removed-but-not-reinstalled skill (DESIGN.md UX #6).
///
/// Separate from `list_trash` because the two outlive each other: a purged
/// skill has no trash unit left, and a tombstone is then the only handle the
/// panel has on it. Listing only units would make the reclaimed skills -- the
/// exact rows tombstones exist for -- invisible.
#[tauri::command]
async fn list_tombstones(trash: tauri::State<'_, SharedTrashStore>) -> Result<Vec<TombstoneReport>, String> {
    let trash = trash.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let store = trash.lock().expect("trash mutex poisoned");
        store.tombstones().map(|ts| ts.iter().map(TombstoneReport::from).collect()).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Reclaims one trashed unit's bytes and reports what it freed. The user's
/// explicit say-so is the only thing that reclaims anything (ADR 0029); nothing
/// here runs on a timer.
///
/// Errors on a `disabled` unit rather than obliging: retained indefinitely means
/// indefinitely.
#[tauri::command]
async fn purge_trash_unit(unit_id: i64, trash: tauri::State<'_, SharedTrashStore>) -> Result<u64, String> {
    let trash = trash.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut store = trash.lock().expect("trash mutex poisoned");
        removal::purge(&mut store, TrashUnitId(unit_id)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Reclaims every trashed unit, skipping disabled ones, and reports the real
/// total rather than the figure the panel offered.
#[tauri::command]
async fn empty_trash(trash: tauri::State<'_, SharedTrashStore>) -> Result<PurgeSummary, String> {
    let trash = trash.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut store = trash.lock().expect("trash mutex poisoned");
        removal::empty_trash(&mut store).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The user's usage-toast settings (issue #14): the rolling-24h budget on/off +
/// limit, and the anomaly toggle. Read on panel open so the settings pane
/// reflects the stored config.
#[tauri::command]
async fn get_usage_settings(adapter: tauri::State<'_, SharedAdapter>) -> Result<UsageSettings, String> {
    let adapter = adapter.inner().clone();
    tauri::async_runtime::spawn_blocking(move || adapter.lock().expect("adapter mutex poisoned").get_usage_settings())
        .await
        .map_err(|e| e.to_string())
}

/// Persist the usage-toast settings (issue #14). Queues behind the scan mutex;
/// the writes are sub-millisecond `usage_meta` upserts. Changing the limit or
/// the enabled flag re-arms the budget debounce inside the adapter (D5).
#[tauri::command]
async fn set_usage_settings(
    settings: UsageSettings,
    adapter: tauri::State<'_, SharedAdapter>,
) -> Result<(), String> {
    let adapter = adapter.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        adapter.lock().expect("adapter mutex poisoned").set_usage_settings(&settings)
    })
    .await
    .map_err(|e| e.to_string())
}

/// Detached background fill of every pending on-demand ceiling (issue #11).
/// Emits `on-demand-ready` exactly once, only when it wrote at least one
/// ceiling, so the panel reloads precisely when there is something new to
/// resolve; the fill is idempotent, so that reload's rescan finds nothing
/// pending and the cycle terminates after one extra pass.
fn spawn_on_demand_fill(app: tauri::AppHandle, adapter: SharedAdapter) {
    tauri::async_runtime::spawn_blocking(move || {
        // Cheap step under the scan lock: the worklist only, no tokenization.
        let pending = {
            let adapter = adapter.lock().expect("adapter mutex poisoned");
            adapter.pending_on_demand()
        };
        if pending.is_empty() {
            return;
        }

        // The background's own connections, opened from the app data dir. A
        // panic in any of these can't poison the interactive adapter's Mutex
        // because none of them is that adapter. If a handle can't be built the
        // fill simply no-ops; the next scan re-pends and tries again.
        let Ok(data_dir) = app.path().app_data_dir() else { return };
        let Ok(cache) = TokenCache::open(&data_dir.join("footprint.sqlite")) else { return };
        let Ok(on_demand_cache) = SqliteOnDemandCache::open(&data_dir.join("on_demand_index.sqlite")) else {
            return;
        };
        let Ok(store) = KeychainApiKeyStore::new() else { return };
        let client = AnthropicCountTokensClient::new();

        let wrote = fill_on_demand_ceilings(&pending, &cache, &on_demand_cache, &store, &client, &BpeTokenizer);
        if wrote {
            let _ = app.emit("on-demand-ready", ());
        }
    });
}

/// Validate and store a user-supplied API key (issue #4). Runs on the blocking
/// pool because both the validating `count_tokens` probe and the keychain
/// write can block; locks only the settings `Mutex`, never the scan one, so a
/// Save can never freeze behind an in-flight scan. Returns a `SetKeyOutcome`
/// (stored / stored-unverified / rejected) so a mistyped key gets feedback
/// instead of silently yielding estimates. The key never crosses back to the
/// UI and never appears in a log or an error string.
#[tauri::command]
async fn set_api_key(key: String, settings: tauri::State<'_, SharedApiKeySettings>) -> Result<SetKeyOutcome, String> {
    let settings = settings.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = settings.lock().expect("api key settings mutex poisoned");
        api_key_service::set_api_key(&key, settings.store.as_ref(), settings.client.as_ref(), REFERENCE_MODEL_ID)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Forget the stored API key (issue #4). Idempotent, and deliberately does NOT
/// purge already-computed exact counts from the footprint cache -- those stay
/// true until their skill content changes (ADR 0023); removal only stops NEW
/// exact counts.
#[tauri::command]
async fn delete_api_key(settings: tauri::State<'_, SharedApiKeySettings>) -> Result<(), String> {
    let settings = settings.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = settings.lock().expect("api key settings mutex poisoned");
        settings.store.delete().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Show or hide the tray panel, with the macOS double-toggle guard.
///
/// Shared by the tray left-click and the global hotkey. If the panel is up,
/// hide it. If it is down, anchor it under the tray icon and show it -- UNLESS
/// a blur-driven hide fired in the last 200ms. That means this very click is
/// the one that dismissed an already-open panel: on macOS the click first
/// steals focus from the panel (firing `Focused(false)`, which hides it) and
/// only then arrives as a tray Click, so without the guard the toggle would
/// see a hidden window and immediately re-show the panel the user meant to
/// close. `Instant`/`Duration` are real wall-clock on purpose -- this is UI
/// input timing, unrelated to the injected-"now" the scan core stays clockless
/// about.
fn toggle_panel(app: &tauri::AppHandle, last_blur_hide: &Arc<Mutex<Instant>>) {
    use tauri_plugin_positioner::{Position, WindowExt};
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    if last_blur_hide.lock().expect("last_blur_hide mutex poisoned").elapsed() < Duration::from_millis(200) {
        return;
    }
    let _ = window.move_window(Position::TrayBottomCenter);
    let _ = window.show();
    let _ = window.set_focus();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    // single-instance must be registered FIRST so a second launch focuses the
    // running tray app instead of spawning a duplicate.
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }));
    }

    builder = builder
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init());

    #[cfg(desktop)]
    {
        builder = builder
            .plugin(tauri_plugin_global_shortcut::Builder::new().build())
            .plugin(tauri_plugin_autostart::Builder::new().build());
    }

    // Shared timestamp for the macOS double-toggle guard (see toggle_panel):
    // the blur handler stamps it the instant it hides the panel, and the tray
    // toggle reads it to swallow the dismiss-click that would otherwise re-open
    // the panel the user just closed. Cloned into the window-event handler and
    // moved into `.setup` for the tray + hotkey toggles.
    let last_blur_hide = Arc::new(Mutex::new(Instant::now()));

    // Blur-to-dismiss: clicking anywhere outside the panel drops its focus,
    // which hides it -- the standard menu-bar dropdown behavior. Stamp
    // last_blur_hide BEFORE hiding so a tray click that triggered this blur is
    // recognizable as a dismiss rather than a fresh open.
    let blur_guard = last_blur_hide.clone();
    builder = builder.on_window_event(move |window, event| {
        if window.label() == "main" {
            if let tauri::WindowEvent::Focused(false) = event {
                *blur_guard.lock().expect("last_blur_hide mutex poisoned") = Instant::now();
                let _ = window.hide();
            }
        }
    });

    builder
        .setup(move |app| {
            // Core tray icon (no plugin — TrayIconBuilder lives in tauri core).
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
            // The tray glyph is a macOS *template* image: pre-decoded raw RGBA
            // (44x44) compiled straight in. Raw bytes rather than the source
            // PNG so there is no runtime decode (which would pull the whole
            // `image` crate in for one icon) and no resource-path lookup that
            // could fail at launch. Regenerate icons/tray.rgba from tray.png if
            // the glyph changes. icon_as_template(true) lets macOS recolor it
            // for light and dark menu bars.
            const TRAY_ICON_RGBA: &[u8] = include_bytes!("../icons/tray.rgba");
            let tray_guard = last_blur_hide.clone();
            let _tray = TrayIconBuilder::with_id("skillmon")
                .icon(tauri::image::Image::new(TRAY_ICON_RGBA, 44, 44))
                .icon_as_template(true)
                .tooltip("skillmon")
                .on_tray_icon_event(move |tray, event| {
                    // anchor the panel to the tray icon on both OSes
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    // Left-click *release* toggles the panel: matching the
                    // button-up fires once per click and ignores a press that
                    // drags off the icon. (MouseButtonState's Up/Down doc
                    // comments are swapped upstream; Up is the release.)
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_panel(tray.app_handle(), &tray_guard);
                    }
                })
                .build(app)?;

            // macOS: run as a menu-bar accessory (no Dock icon).
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Composition root: build the one adapter the whole app shares.
            // The footprint cache is a single SQLite file in the app data dir
            // (ADR 0008); the API key comes from the OS keychain (ADR 0020);
            // count_tokens goes to the real Anthropic endpoint (ADR 0006).
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let cache = TokenCache::open(&data_dir.join("footprint.sqlite"))?;
            // Sibling of footprint.sqlite: the per-transcript listing memo that
            // makes a warm rescan skip re-reading unchanged transcripts, and
            // survives restart so the first panel-open after login is fast too
            // (ADR 0022). Kept in its own file because it holds Claude-Code
            // transcript data, not harness-neutral token counts (ADR 0002).
            let listing_cache = SqliteListingCache::open(&data_dir.join("listing_index.sqlite"))?;
            // Attributed-usage store (issue #5): a third persisted sqlite file
            // beside the footprint and listing caches, holding deduped
            // per-message usage keyed by message.id (ADR 0024).
            let usage_cache = SqliteUsageCache::open(&data_dir.join("usage.sqlite"))?;
            // Fourth persisted sqlite file (issue #11): the per-skill on-demand
            // ceiling memo, opened WAL so the background fill's separate
            // connection to the same file never blocks or poisons this one.
            let on_demand_cache = SqliteOnDemandCache::open(&data_dir.join("on_demand_index.sqlite"))?;
            // Fifth persisted sqlite file (issue #28): the trash ledger. Unlike
            // the four above it is NOT a cache -- nothing can re-derive where a
            // trashed entry came from, so it never wipes on a version bump and
            // losing it would strand real files with no undo (ADR 0029).
            let trash_store = TrashStore::open(&data_dir.join("removal.sqlite"))?;
            let adapter = ClaudeCodeAdapter::new(
                default_claude_home(),
                cache,
                listing_cache,
                usage_cache,
                on_demand_cache,
                Box::new(KeychainApiKeyStore::new()?),
                Box::new(AnthropicCountTokensClient::new()),
                Box::new(BpeTokenizer),
            );

            // ADR 0018/0019: exact counts measured against a superseded
            // reference model are recounted on the next scan (compute
            // self-heals a stale model). Report the count at startup so a
            // model bump is observable rather than silent.
            let stale = adapter.stale_exact_count();
            if stale > 0 {
                eprintln!(
                    "[skillmon] {stale} cached exact footprint(s) predate the current reference model; \
                     they will be recounted on the next scan"
                );
            }

            let adapter: SharedAdapter = Arc::new(Mutex::new(adapter));

            // Registry watcher (ADR 0019): a debounced change to any watched
            // surface emits `registry-changed`; the UI listens and re-invokes
            // `list_skills`. Enablement is read at session start, so this is a
            // "your skill list may be stale" nudge, not a live-state mirror.
            let app_handle = app.handle().clone();
            let watcher = RegistryWatcher::new(move || {
                let _ = app_handle.emit("registry-changed", ());
            })?;
            // Issue #29: the latch every mutation holds while it writes inside
            // the scan root the watcher above watches recursively. Taken from
            // the watcher rather than built here -- only it knows how long a
            // tail its own debounce needs. Managed separately from the watcher's
            // Mutex on purpose: a mutation must never queue behind a scan's
            // `sync_watcher` just to say "this write is mine", and the notify
            // callback must never block on a lock a mutation is holding.
            let self_writes = watcher.self_writes();
            let watcher: SharedWatcher = Arc::new(Mutex::new(watcher));

            // Watch the static global surfaces plus any repos already known at
            // launch. Later scans re-sync to pick up repos that appear after.
            adapter
                .lock()
                .expect("adapter mutex poisoned")
                .sync_watcher(&mut watcher.lock().expect("watcher mutex poisoned"));

            // API-key settings (issue #4): a second, independent handle over
            // the same keychain entry the adapter reads, managed on its own so
            // set/delete never lock the scan Mutex (see ApiKeySettings).
            let api_key_settings: SharedApiKeySettings = Arc::new(Mutex::new(ApiKeySettings {
                store: Box::new(KeychainApiKeyStore::new()?),
                client: Box::new(AnthropicCountTokensClient::new()),
            }));

            app.manage(adapter);
            app.manage(watcher);
            app.manage(api_key_settings);
            app.manage(self_writes);
            app.manage::<SharedTrashStore>(Arc::new(Mutex::new(trash_store)));
            app.manage::<SharedManagingTools>(Arc::new(ManagingTools::from_env()));

            // Global hotkey: Cmd/Ctrl+Shift+K toggles the panel from anywhere.
            // A conflict (another app already owns the combo) is non-fatal --
            // log it and carry on, never crash the tray over a hotkey.
            #[cfg(desktop)]
            {
                use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
                let shortcut_guard = last_blur_hide.clone();
                if let Err(e) = app.global_shortcut().on_shortcut("CommandOrControl+Shift+K", move |app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        toggle_panel(app, &shortcut_guard);
                    }
                }) {
                    eprintln!("[skillmon] global shortcut CommandOrControl+Shift+K not registered (already taken?): {e}");
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_skills,
            set_api_key,
            delete_api_key,
            get_usage_settings,
            set_usage_settings,
            list_trash,
            list_tombstones,
            restore_trash_unit,
            purge_trash_unit,
            empty_trash
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
