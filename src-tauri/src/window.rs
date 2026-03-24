use anyhow::Result;
use std::time::Duration;
use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use super::{AudioLevelPayload, NeumaState};

// ─── Accessibility + Input Monitoring ────────────────────────────────────────

// macOS 14.2+ added CGPreflightListenEventAccess / CGRequestListenEventAccess
// as the correct APIs for kCGEventTapOptionListenOnly-based CGEventTap access.
// On macOS 14.2+, IOHIDCheckAccess/IOHIDRequestAccess no longer add the app
// to the Input Monitoring list for CG-tap-based listeners — only the CG APIs do.
// We resolve them at runtime via dlsym so the binary still runs on older macOS.
#[cfg(target_os = "macos")]
extern "C" {
    fn dlsym(handle: *mut std::ffi::c_void, symbol: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
}
// RTLD_DEFAULT = (void*)-2 on macOS
#[cfg(target_os = "macos")]
const RTLD_DEFAULT: *mut std::ffi::c_void = (-2isize) as *mut std::ffi::c_void;

#[cfg(target_os = "macos")]
fn cg_preflight_listen_event() -> Option<bool> {
    let sym = unsafe { dlsym(RTLD_DEFAULT, b"CGPreflightListenEventAccess\0".as_ptr() as *const _) };
    if sym.is_null() { return None; }
    let f: extern "C" fn() -> bool = unsafe { std::mem::transmute(sym) };
    Some(f())
}

#[cfg(target_os = "macos")]
fn cg_request_listen_event() -> Option<()> {
    let sym = unsafe { dlsym(RTLD_DEFAULT, b"CGRequestListenEventAccess\0".as_ptr() as *const _) };
    if sym.is_null() { return None; }
    let f: extern "C" fn() -> bool = unsafe { std::mem::transmute(sym) };
    f();
    Some(())
}

/// Returns true if the process has the permission needed for a listen-only CGEventTap.
///
/// On macOS 14.2+: checks `CGPreflightListenEventAccess` (Input Monitoring).
/// On older macOS:  checks `AXIsProcessTrusted`            (Accessibility).
pub(crate) fn listening_permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(granted) = cg_preflight_listen_event() {
            return granted;
        }
        ax_is_process_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    { true }
}

/// Which TCC permission Neuma needs on this macOS version.
/// "input_monitoring" on macOS 14.2+, "accessibility" on older macOS.
pub(crate) fn listening_permission_type() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        let sym = unsafe { dlsym(RTLD_DEFAULT, b"CGRequestListenEventAccess\0".as_ptr() as *const _) };
        if !sym.is_null() { "input_monitoring" } else { "accessibility" }
    }
    #[cfg(not(target_os = "macos"))]
    { "none" }
}

/// Requests the permission needed for a listen-only CGEventTap and opens the
/// relevant System Settings pane.
///
/// On macOS 14.2+: calls `CGRequestListenEventAccess` → opens Input Monitoring.
/// On older macOS:  calls `AXIsProcessTrustedWithOptions` → opens Accessibility.
pub(crate) fn request_listening_permission() {
    #[cfg(target_os = "macos")]
    {
        if cg_request_listen_event().is_some() {
            // macOS 14.2+: Input Monitoring
            let _ = std::process::Command::new("open")
                .arg("x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_ListenEvent")
                .spawn();
        } else {
            // Pre-14.2: Accessibility
            ax_request_trust();
        }
    }
}

// CoreFoundation imports used by `ax_request_trust`.
// CFDictionaryCreate lets us build the options dict without an autorelease
// pool — required because Tauri commands run on worker threads.
#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: *const std::ffi::c_void;
    // Address-only; we only ever pass `&kCFTypeDictionary*Callbacks as *const _`.
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    // Return type is *mut to match the existing declaration in ax.rs.
    fn CFStringCreateWithCString(
        alloc: *const std::ffi::c_void,
        c_str: *const std::os::raw::c_char,
        encoding: u32,
    ) -> *mut std::ffi::c_void;
    fn CFDictionaryCreate(
        alloc: *const std::ffi::c_void,
        keys: *const *const std::ffi::c_void,
        values: *const *const std::ffi::c_void,
        num_values: isize,
        key_callbacks: *const u8,
        value_callbacks: *const u8,
    ) -> *const std::ffi::c_void;
    fn CFRelease(cf: *const std::ffi::c_void);
}

// ApplicationServices import for the AX trust prompt.
#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

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

/// Calls `AXIsProcessTrustedWithOptions({kAXTrustedCheckOptionPrompt: true})`.
///
/// This is the *only* reliable way on macOS 13+ to add the app to the
/// System Settings → Accessibility list. CGEventTapCreate returning null
/// does NOT register the app. Without this call Neuma never appears in the
/// list and the user cannot grant the permission.
///
/// Uses CoreFoundation (`CFDictionaryCreate`) instead of ObjC/NSDict because
/// CF retain/release are thread-safe and require no autorelease pool — Tauri
/// commands run on worker threads where autorelease pools are absent.
///
/// After calling the API we also open the System Settings pane so the user
/// sees Neuma in the list immediately. No-op on non-macOS.
pub(crate) fn ax_request_trust() {
    #[cfg(target_os = "macos")]
    {
        // kCFStringEncodingUTF8 = 0x0800_0100
        const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

        let prompted = unsafe {
            let key = CFStringCreateWithCString(
                std::ptr::null(),
                b"AXTrustedCheckOptionPrompt\0".as_ptr() as *const _,
                CF_STRING_ENCODING_UTF8,
            );
            if key.is_null() {
                false
            } else {
                let keys: [*const std::ffi::c_void; 1] = [key];
                let values: [*const std::ffi::c_void; 1] = [kCFBooleanTrue];
                let dict = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    values.as_ptr(),
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                );
                CFRelease(key);
                if dict.is_null() {
                    false
                } else {
                    AXIsProcessTrustedWithOptions(dict);
                    CFRelease(dict);
                    true
                }
            }
        };

        // Also open the pane so the user sees Neuma in the list right away.
        // On macOS 13+ this navigates directly to Privacy → Accessibility.
        let _ = std::process::Command::new("open")
            .arg(if prompted {
                // System Settings (Ventura+)
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility"
            } else {
                // Fallback for older macOS
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            })
            .spawn();
    }
}


// ─── Microphone permission (AVFoundation) ────────────────────────────────────
//
// AVCaptureDevice.authorizationStatus / requestAccess via ObjC FFI.
// Safe to call from any thread; request_mic_permission blocks via Condvar and
// must be run inside tokio::task::spawn_blocking.

// Link AVFoundation so the ObjC runtime knows about AVCaptureDevice.
// Without this, objc_getClass("AVCaptureDevice") returns nil and all
// mic permission calls silently fail.
#[cfg(target_os = "macos")]
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

#[derive(Debug, PartialEq, Eq, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MicStatus {
    Authorized,
    NotDetermined,
    Denied,
    Restricted,
}

/// Returns the current microphone authorization status.
/// Non-blocking — safe to call from the CGEventTap callback thread.
pub(crate) fn mic_permission_status() -> MicStatus {
    #[cfg(target_os = "macos")]
    {
        use objc::{class, msg_send, sel, sel_impl};
        use std::ffi::c_void;

        // AVMediaTypeAudio is an NSString constant.
        // We retrieve it via NSString literal "soun" (the 4-char code).
        // Simpler: use the string directly as a class method argument.
        // authorizationStatusForMediaType: returns AVAuthorizationStatus (i64):
        //   0 = NotDetermined, 1 = Restricted, 2 = Denied, 3 = Authorized
        let status: i64 = unsafe {
            let av_class = class!(AVCaptureDevice);
            // AVMediaTypeAudio as an NSString pointer
            let media_type: *mut objc::runtime::Object = msg_send![
                class!(NSString),
                stringWithUTF8String: b"soun\0".as_ptr() as *const std::os::raw::c_char
            ];
            msg_send![av_class, authorizationStatusForMediaType: media_type as *mut c_void]
        };

        let result = match status {
            3 => MicStatus::Authorized,
            1 => MicStatus::Restricted,
            2 => MicStatus::Denied,
            _ => MicStatus::NotDetermined,
        };
        log::info!("mic_permission_status: raw={status} → {result:?}");
        result
    }
    #[cfg(not(target_os = "macos"))]
    {
        MicStatus::Authorized
    }
}

/// Requests microphone access via AVCaptureDevice.
/// Blocks the calling thread until the user responds to the system dialog.
/// Must be called from tokio::task::spawn_blocking, never from the CGEventTap thread.
/// Returns true if access was granted.
pub(crate) fn request_mic_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc::{class, msg_send, sel, sel_impl};
        use std::ffi::c_void;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Static atomics: safe because request_mic_permission is only ever called
        // when status == NotDetermined, which can only be true once per process.
        static DONE: AtomicBool = AtomicBool::new(false);
        static GRANTED: AtomicBool = AtomicBool::new(false);
        DONE.store(false, Ordering::SeqCst);
        GRANTED.store(false, Ordering::SeqCst);

        // Build a minimal ObjC stack block (Apple Block ABI).
        // The block captures nothing — it only writes to the static atomics above.
        // AVFoundation copies the block to the heap internally; the invoke fn pointer
        // remains valid (it's a static fn), and it only touches static data, so there
        // are no lifetime or aliasing issues.
        #[repr(C)]
        struct Block {
            isa: *const c_void,
            flags: i32,
            reserved: i32,
            invoke: extern "C" fn(*mut Block, bool),
            descriptor: *const BlockDescriptor,
        }
        #[repr(C)]
        struct BlockDescriptor {
            reserved: usize,
            size: usize,
        }
        static DESCRIPTOR: BlockDescriptor = BlockDescriptor {
            reserved: 0,
            size: std::mem::size_of::<Block>(),
        };
        extern "C" fn block_invoke(_block: *mut Block, granted: bool) {
            GRANTED.store(granted, Ordering::SeqCst);
            DONE.store(true, Ordering::SeqCst);
        }
        extern "C" {
            static _NSConcreteStackBlock: c_void;
        }

        let mut block = Block {
            isa: unsafe { &_NSConcreteStackBlock as *const c_void },
            flags: 0,
            reserved: 0,
            invoke: block_invoke,
            descriptor: &DESCRIPTOR,
        };

        unsafe {
            let av_class = class!(AVCaptureDevice);
            let media_type: *mut objc::runtime::Object = msg_send![
                class!(NSString),
                stringWithUTF8String: b"soun\0".as_ptr() as *const std::os::raw::c_char
            ];
            let _: () = msg_send![
                av_class,
                requestAccessForMediaType: media_type as *mut c_void
                completionHandler: &mut block as *mut Block as *mut c_void
            ];
        }

        // Spin-wait with sleep — keeps the thread parked while AVFoundation
        // shows the dialog. Completion fires on AVFoundation's internal queue.
        while !DONE.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        GRANTED.load(Ordering::SeqCst)
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Opens System Settings → Microphone so the user can toggle Neuma's access.
pub(crate) fn open_mic_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Microphone")
            .spawn();
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
