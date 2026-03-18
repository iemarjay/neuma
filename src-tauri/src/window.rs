use anyhow::Result;
use std::time::Duration;
use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use super::{AudioLevelPayload, NeumaState};

// ─── Accessibility ────────────────────────────────────────────────────────────

/// Returns true if the process has been granted Accessibility (AX) permission.
/// On non-macOS platforms always returns true (not applicable).
pub(crate) fn ax_is_process_trusted() -> bool {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        unsafe { AXIsProcessTrusted() }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

// ─── Event emitters ───────────────────────────────────────────────────────────

pub(crate) fn emit_state(app: &tauri::AppHandle, state: NeumaState) {
    if let Err(e) = app.emit("neuma://state", state) {
        log::error!("failed to emit state event: {e}");
    }
}

pub(crate) fn emit_audio_level(app: &tauri::AppHandle, level: f32) {
    if let Err(e) = app.emit("neuma://audio-level", AudioLevelPayload { level }) {
        log::error!("failed to emit audio-level event: {e}");
    }
}

/// Emit the error overlay state and send a system notification with the same message.
pub(crate) fn emit_error(app: &tauri::AppHandle, message: &str) {
    emit_state(
        app,
        NeumaState::Error {
            message: message.to_string(),
        },
    );
    if let Err(e) = app
        .notification()
        .builder()
        .title("Neuma")
        .body(message)
        .show()
    {
        log::warn!("failed to send error notification: {e}");
    }
}

// ─── Window creation ──────────────────────────────────────────────────────────

/// Creates the overlay window hidden. Called once at startup so the first
/// hotkey press only needs to `.show()` it — no WebView creation latency.
pub(crate) fn create_overlay_window(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow> {
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "overlay",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("Neuma")
    .inner_size(220.0, 38.0)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible(false)
    .center()
    .visible_on_all_workspaces(true)
    .build()?;

    Ok(win)
}

/// Creates the branded startup window. Visible on launch, auto-dismissed by
/// the frontend once the Whisper model is ready (or missing → shows download UI).
pub(crate) fn create_startup_window(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow> {
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "startup",
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("Neuma")
    .inner_size(380.0, 380.0)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .center()
    .build()?;

    Ok(win)
}

// ─── macOS window pinning ─────────────────────────────────────────────────────

/// Collection behavior applied to every Neuma window.
///
///   1      = NSWindowCollectionBehaviorCanJoinAllSpaces
///   64     = NSWindowCollectionBehaviorIgnoresCycle (stay out of Cmd+Tab)
///   256    = NSWindowCollectionBehaviorFullScreenAuxiliary
///   262144 = NSWindowCollectionBehaviorCanJoinAllApplications (macOS 13+)
const COLLECTION_BEHAVIOR: u64 = 1 | 64 | 256 | 262144; // = 262465

/// Pins a window to all Spaces and sets it up to appear in fullscreen Spaces.
/// Must be called after the event loop is running (RunEvent::Ready), not during
/// setup() — setCollectionBehavior called before the run loop starts is ignored.
#[cfg(target_os = "macos")]
pub(crate) fn pin_to_all_spaces(window: &tauri::WebviewWindow) {
    use objc::{msg_send, sel, sel_impl};
    if let Ok(ptr) = window.ns_window() {
        let ns_window = ptr as *mut objc::runtime::Object;
        unsafe {
            let _: () = msg_send![ns_window, setCollectionBehavior: COLLECTION_BEHAVIOR];
        }
    }
}

/// Shows the overlay above fullscreen content.
///
/// Sets screenSaver-level z-order and calls `orderFrontRegardless` which —
/// unlike `makeKeyAndOrderFront` — crosses into fullscreen Spaces without
/// needing the window to become the key window.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)]
pub(crate) fn set_macos_overlay_level(window: &tauri::WebviewWindow) {
    use objc::{msg_send, sel, sel_impl};
    if let Ok(ptr) = window.ns_window() {
        let ns_window = ptr as *mut objc::runtime::Object;
        unsafe {
            let _: () = msg_send![ns_window, setLevel: 1000_i64];
            let _: () = msg_send![ns_window, setCollectionBehavior: COLLECTION_BEHAVIOR];
            let _: () = msg_send![ns_window, orderFrontRegardless];
        }
    }
}

/// Returns the height of the dock inset in logical points on macOS.
/// Uses `NSScreen.mainScreen.visibleFrame.origin.y` which is the distance
/// from the bottom of the screen to the bottom of the usable area —
/// i.e. the dock height when visible, near-zero when auto-hidden.
#[cfg(target_os = "macos")]
fn dock_bottom_inset_pts() -> f64 {
    use objc::{class, msg_send, sel, sel_impl};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSRect {
        origin: NSPoint,
        size: NSSize,
    }

    unsafe {
        let ns_screen: *mut objc::runtime::Object = msg_send![class!(NSScreen), mainScreen];
        if ns_screen.is_null() {
            return 0.0;
        }
        let visible: NSRect = msg_send![ns_screen, visibleFrame];
        visible.origin.y // points from screen bottom = dock height (0 when auto-hidden)
    }
}

pub(crate) fn position_overlay_bottom_center(app: &tauri::AppHandle) -> Result<()> {
    use tauri::LogicalPosition;

    let window = app
        .get_webview_window("overlay")
        .ok_or_else(|| anyhow::anyhow!("overlay window not found"))?;

    let monitors = window.available_monitors()?;
    let cursor = window.cursor_position().ok();

    let target_monitor = cursor
        .and_then(|c| {
            monitors.iter().find(|m| {
                let pos = m.position();
                let size = m.size();
                c.x >= pos.x as f64
                    && c.x < (pos.x + size.width as i32) as f64
                    && c.y >= pos.y as f64
                    && c.y < (pos.y + size.height as i32) as f64
            })
        })
        .cloned()
        .or_else(|| window.primary_monitor().ok().flatten());

    if let Some(monitor) = target_monitor {
        let scale = monitor.scale_factor();
        let pos = monitor.position();
        let screen_size = monitor.size();

        let monitor_x = pos.x as f64 / scale;
        let monitor_y = pos.y as f64 / scale;
        let screen_w = screen_size.width as f64 / scale;
        let screen_h = screen_size.height as f64 / scale;

        let win_size = window.outer_size()?;
        let win_w = win_size.width as f64 / scale;
        let win_h = win_size.height as f64 / scale;

        // Dock-aware bottom inset: visibleFrame.origin.y gives dock height in
        // logical points (near-zero when dock is auto-hidden). Add a small gap.
        #[cfg(target_os = "macos")]
        let bottom_inset = dock_bottom_inset_pts() + 8.0;
        #[cfg(not(target_os = "macos"))]
        let bottom_inset = 40.0;

        let x = monitor_x + (screen_w - win_w) / 2.0;
        let y = monitor_y + screen_h - win_h - bottom_inset;

        window.set_position(LogicalPosition::new(x, y))?;
    }

    Ok(())
}

pub(crate) fn hide_overlay_after_delay(app: tauri::AppHandle, delay: Duration) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        if let Some(win) = app.get_webview_window("overlay") {
            let _ = win.hide();
        }
        emit_state(&app, NeumaState::Idle);
    });
}

// ─── Settings window ──────────────────────────────────────────────────────────

pub(crate) fn show_or_create_settings_window(app: &tauri::AppHandle) -> Result<()> {
    if let Some(win) = app.get_webview_window("settings") {
        win.show()?;
        win.set_focus()?;
    } else {
        let win = tauri::WebviewWindowBuilder::new(
            app,
            "settings",
            tauri::WebviewUrl::App("index.html".into()),
        )
        .title("Neuma Settings")
        .inner_size(440.0, 460.0)
        .resizable(false)
        .center()
        .build()?;

        #[cfg(target_os = "macos")]
        pin_to_all_spaces(&win);

        let win_clone = win.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = win_clone.hide();
            }
        });
    }
    Ok(())
}
