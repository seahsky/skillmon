// skillmon — Tauri core entry point.
// Menu-bar (macOS) / system-tray (Windows) app. Domain language: ../CONTEXT.md.
// Architecture and decisions: ../../docs/DESIGN.md, ../../docs/adr/.

mod adapters;
mod domain;
mod footprint;

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
use domain::report::{ScanReport, UsageSettings};
use domain::scan::{ScanParams, UsageWindow};
use footprint::api_key_service::{self, SetKeyOutcome};
use footprint::api_key_store::{ApiKeyStore, KeychainApiKeyStore};
use footprint::cache::TokenCache;
use footprint::count_tokens_client::{AnthropicCountTokensClient, CountTokensClient};
use footprint::tokenizer::BpeTokenizer;

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

// TODO(skillmon): remaining IPC surface (disable/enable, uninstall,
// remove_plugin) lands with later plans.

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

    Ok(outcome.report)
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
            set_usage_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
