use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_store::StoreExt;

use super::{ExplicitQuit, NeumaState, SharedState};
use super::window::{ax_is_process_trusted, emit_state, show_or_create_settings_window};

// ─── TrayState ────────────────────────────────────────────────────────────────

/// Keeps the tray icon and its live menu items accessible after setup.
pub(crate) struct TrayState {
    pub(crate) tray: tauri::tray::TrayIcon,
    pub(crate) launch_at_login_item: CheckMenuItem<tauri::Wry>,
    /// Present only when Accessibility was NOT granted at startup (macOS).
    pub(crate) ax_item: Option<MenuItem<tauri::Wry>>,
}

// ─── Builder ──────────────────────────────────────────────────────────────────

pub(crate) fn build_tray(
    app: &tauri::AppHandle,
    _state: SharedState,
    ax_trusted: bool,
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

    let (menu, ax_item_opt) = if !ax_trusted {
        let ax_item = MenuItem::with_id(
            app,
            "open_accessibility",
            "⚠ Grant Accessibility Access",
            true,
            None::<&str>,
        )?;
        let ax_sep = PredefinedMenuItem::separator(app)?;
        let m = Menu::with_items(
            app,
            &[&ax_item, &ax_sep, &settings_item, &launch_item, &sep, &quit_item],
        )?;
        (m, Some(ax_item))
    } else {
        let m = Menu::with_items(app, &[&settings_item, &launch_item, &sep, &quit_item])?;
        (m, None)
    };

    let tooltip = if ax_trusted {
        "Neuma — click to open menu"
    } else {
        "Neuma — ⚠ Accessibility not granted"
    };

    let tray_icon_bytes = include_bytes!("../icons/tray-icon.png");
    let tray_image = tauri::image::Image::from_bytes(tray_icon_bytes)
        .unwrap_or_else(|_| app.default_window_icon().unwrap().clone());

    // Flips to true after AX is granted mid-session (restart required).
    let ax_restart_required = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ax_restart_for_event = Arc::clone(&ax_restart_required);

    let launch_item_clone = launch_item.clone();
    let ax_item_clone = ax_item_opt.clone();

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
            "open_accessibility" => {
                if ax_restart_for_event.load(Ordering::Relaxed) {
                    app.exit(0);
                } else {
                    #[cfg(target_os = "macos")]
                    let _ = std::process::Command::new("open")
                        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                        .spawn();
                }
            }
            _ => {}
        })
        .build(app)?;

    // AX poll — if user grants permission after launch, prompt restart.
    if !ax_trusted {
        let app_poll = app.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if ax_is_process_trusted() {
                    ax_restart_required.store(true, Ordering::Relaxed);
                    if let Some(ts) = app_poll.try_state::<Mutex<TrayState>>() {
                        if let Ok(ts) = ts.lock() {
                            if let Some(ref item) = ts.ax_item {
                                let _ = item.set_text("↺ Restart Neuma to activate hotkey");
                            }
                            let _ = ts.tray.set_tooltip(Some("Neuma — restart to activate hotkey"));
                        }
                    }
                    // Emit Idle so the overlay doesn't stay stuck if it was showing
                    emit_state(&app_poll, NeumaState::Idle);
                    break;
                }
            }
        });
    }

    Ok(TrayState {
        tray,
        launch_at_login_item: launch_item,
        ax_item: ax_item_clone,
    })
}
