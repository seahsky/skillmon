// skillmon — Tauri core entry point.
// Menu-bar (macOS) / system-tray (Windows) app. Domain language: ../CONTEXT.md.
// Architecture and decisions: ../../docs/DESIGN.md, ../../docs/adr/.

mod domain;

use tauri::Manager;

// TODO(skillmon): replace this demo command with the real IPC surface
// (list_skills, footprint, attributed_usage, disable/enable, uninstall, remove_plugin).
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
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
            let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // TODO(skillmon): apply vibrancy (macOS) / Mica (Windows 11), open the
            // rusqlite store, start the notify file watcher over ~/.claude, and
            // manage a shared reqwest client for count_tokens.
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
