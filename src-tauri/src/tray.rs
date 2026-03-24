use std::sync::atomic::Ordering;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_store::StoreExt;

use super::{ExplicitQuit, SharedState};
use super::window::{request_listening_permission, show_or_create_settings_window};

// ─── TrayState ────────────────────────────────────────────────────────────────

/// Keeps the tray icon and its live menu items accessible after setup.
pub(crate) struct TrayState {
    #[allow(dead_code)] // must be kept alive to keep the tray icon visible
    pub(crate) tray: tauri::tray::TrayIcon,
    pub(crate) launch_at_login_item: CheckMenuItem<tauri::Wry>,
}

// ─── Builder ──────────────────────────────────────────────────────────────────

pub(crate) fn build_tray(
    app: &tauri::AppHandle,
    _state: SharedState,
    permission_granted: bool,
) -> tauri::Result<TrayState> {
    let autostart_enabled = app.autolaunch().is_enabled().unwrap_or(false);

    let settings_item = MenuItem::with_id(app, "settings", "Settings...", true, None::<&str>)?;
    let launch_item = CheckMenuItem::with_id(
        app,
        "autostart",
        "Launch at Login",
        true,
        autostart_enabled,
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit Neuma", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;

    let (menu, _perm_item) = if !permission_granted {
        let perm_item = MenuItem::with_id(
            app,
            "grant_permission",
            "⚠ Grant Hotkey Access",
            true,
            None::<&str>,
        )?;
        let perm_sep = PredefinedMenuItem::separator(app)?;
        let m = Menu::with_items(
            app,
            &[&perm_item, &perm_sep, &settings_item, &launch_item, &sep, &quit_item],
        )?;
        (m, Some(perm_item))
    } else {
        let m = Menu::with_items(app, &[&settings_item, &launch_item, &sep, &quit_item])?;
        (m, None)
    };

    let tooltip = if permission_granted {
        "Neuma — click to open menu"
    } else {
        "Neuma — ⚠ permission not granted"
    };

    let tray_icon_bytes = include_bytes!("../icons/tray-icon.png");
    let tray_image = tauri::image::Image::from_bytes(tray_icon_bytes)
        .unwrap_or_else(|_| app.default_window_icon().unwrap().clone());

    let launch_item_clone = launch_item.clone();

    let tray = TrayIconBuilder::new()
        .icon(tray_image)
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(true)
        .tooltip(tooltip)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "settings" => {
                let _ = show_or_create_settings_window(app);
            }
            "quit" => {
                if let Some(flag) = app.try_state::<ExplicitQuit>() {
                    flag.0.store(true, Ordering::Relaxed);
                }
                app.exit(0);
            }
            "autostart" => {
                let autolaunch = app.autolaunch();
                let enabled = autolaunch.is_enabled().unwrap_or(false);
                if enabled {
                    let _ = autolaunch.disable();
                    let _ = launch_item_clone.set_checked(false);
                } else {
                    let _ = autolaunch.enable();
                    let _ = launch_item_clone.set_checked(true);
                }
                // Keep settings.json in sync
                if let Some(shared) = app.try_state::<SharedState>() {
                    let mut s = shared.lock().unwrap();
                    s.settings.launch_at_login = !enabled;
                    let new_settings = s.settings.clone();
                    drop(s);
                    if let Ok(store) = app.store("settings.json") {
                        if let Ok(v) = serde_json::to_value(&new_settings) {
                            store.set("settings", v);
                            let _ = store.save();
                        }
                    }
                }
            }
            "grant_permission" => {
                request_listening_permission();
            }
            _ => {}
        })
        .build(app)?;

    Ok(TrayState {
        tray,
        launch_at_login_item: launch_item,
    })
}
