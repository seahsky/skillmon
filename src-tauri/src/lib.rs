// skillmon — Tauri core entry point.
// Menu-bar (macOS) / system-tray (Windows) app. Domain language: ../CONTEXT.md.
// Architecture and decisions: ../../docs/DESIGN.md, ../../docs/adr/.

mod adapters;
mod domain;
mod footprint;

use std::sync::{Arc, Mutex};

use tauri::{Emitter, Manager};

use adapters::claude_code::paths::default_claude_home;
use adapters::claude_code::watcher::RegistryWatcher;
use adapters::claude_code::ClaudeCodeAdapter;
use domain::harness::HarnessAdapter;
use domain::report::ScanReport;
use footprint::api_key_store::KeychainApiKeyStore;
use footprint::cache::TokenCache;
use footprint::count_tokens_client::AnthropicCountTokensClient;

/// The scan orchestration is synchronous (ADR 0008) and can block on file
/// I/O or a `count_tokens` call, so it lives behind a `Mutex` and is only
/// ever touched from the blocking pool. `Arc<Mutex<_>>` is the `Send + Sync`
/// shape Tauri's managed state requires; `TokenCache`'s `rusqlite`
/// connection is `Send` but not `Sync`, which is exactly why the `Mutex` is
/// mandatory, not merely convenient.
type SharedAdapter = Arc<Mutex<ClaudeCodeAdapter>>;
type SharedWatcher = Arc<Mutex<RegistryWatcher>>;

// TODO(skillmon): remaining IPC surface (attributed_usage, disable/enable,
// uninstall, remove_plugin, API-key set/delete) lands with later plans.

/// Discover every skill and compute its three-layer footprint (ADR 0019's
/// scan). The synchronous core runs on the blocking pool via
/// `spawn_blocking` (footprint-counter plan decision #4) so this async
/// command never stalls the runtime on I/O or a network call. Each scan also
/// re-syncs the registry watcher, so a repo that appeared since the last
/// scan starts being watched without waiting for a restart.
#[tauri::command]
async fn list_skills(
    adapter: tauri::State<'_, SharedAdapter>,
    watcher: tauri::State<'_, SharedWatcher>,
) -> Result<ScanReport, String> {
    let adapter = adapter.inner().clone();
    let watcher = watcher.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let adapter = adapter.lock().expect("adapter mutex poisoned");
        let report = adapter.scan_all();
        adapter.sync_watcher(&mut watcher.lock().expect("watcher mutex poisoned"));
        report
    })
    .await
    .map_err(|e| e.to_string())
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

    builder
        .setup(|app| {
            // Core tray icon (no plugin — TrayIconBuilder lives in tauri core).
            use tauri::tray::TrayIconBuilder;
            let _tray = TrayIconBuilder::with_id("skillmon")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("skillmon")
                .on_tray_icon_event(|tray, event| {
                    // anchor the panel to the tray icon on both OSes
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
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
            let adapter = ClaudeCodeAdapter::new(
                default_claude_home(),
                cache,
                Box::new(KeychainApiKeyStore::new()?),
                Box::new(AnthropicCountTokensClient::new()),
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

            app.manage(adapter);
            app.manage(watcher);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![list_skills])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
