#![cfg(target_os = "macos")]

/// Single-key listener with tap-vs-hold detection.
///
/// Replaces the rdev-based implementation which crashes on macOS 26+ due to
/// a bug in rdev's Keyboard::string_from_code (Carbon API incompatibility).
/// Uses CGEventTap directly — monitors kCGEventFlagsChanged for modifier keys
/// (Alt/Option, Ctrl, Fn) with no key-to-string conversion.
///
/// One key, two behaviors decided at release time:
///   Tap  (release < HOLD_THRESHOLD_MS) → toggle mode
///   Hold (release ≥ HOLD_THRESHOLD_MS) → push-to-talk
///
/// Requires Accessibility permission on macOS:
///   System Settings → Privacy & Security → Accessibility

use std::ffi::c_void;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

const HOLD_THRESHOLD_MS: u64 = 400;

// ── CoreGraphics / CoreFoundation FFI ────────────────────────────────────────

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void) -> *mut c_void,
        user_info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetIntegerValueField(event: *mut c_void, field: i32) -> i64;
    fn CGEventGetFlags(event: *mut c_void) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        tap: *mut c_void,
        order: isize,
    ) -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *mut c_void, mode: *const c_void);
    fn CFRunLoopRun();
    fn CFRunLoopGetCurrent() -> *mut c_void;
    static kCFRunLoopCommonModes: *const c_void;
}

// kCGEventFlagsChanged = 12
const CG_EVENT_FLAGS_CHANGED: u32 = 12;
// kCGSessionEventTap = 1
const CG_SESSION_EVENT_TAP: u32 = 1;
// kCGHeadInsertEventTap = 0
const CG_HEAD_INSERT_EVENT_TAP: u32 = 0;
// kCGEventTapOptionListenOnly = 1  (passive — we never modify events)
const CG_EVENT_TAP_OPTION_LISTEN_ONLY: u32 = 1;
// event mask: only kCGEventFlagsChanged
const CG_MASK_FLAGS_CHANGED: u64 = 1 << CG_EVENT_FLAGS_CHANGED;
// kCGKeyboardEventKeycode field index
const CG_KEYBOARD_EVENT_KEYCODE: i32 = 9;
// Tap disabled events
const CG_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFFFFFE;
const CG_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFFFFFF;

// macOS virtual key codes (Carbon kVK_*)
const VK_OPTION: u16 = 58;        // Left Alt / Option
const VK_RIGHT_OPTION: u16 = 61;  // Right Alt / AltGr
const VK_CONTROL: u16 = 59;       // Left Control
const VK_RIGHT_CONTROL: u16 = 62; // Right Control
const VK_FUNCTION: u16 = 63;      // Fn

// CGEventFlags masks
const FLAG_OPTION: u64 = 0x0008_0000;   // kCGEventFlagMaskAlternate
const FLAG_CONTROL: u64 = 0x0004_0000;  // kCGEventFlagMaskControl
const FLAG_FUNCTION: u64 = 0x0080_0000; // kCGEventFlagMaskSecondaryFn

// ── Key parsing ──────────────────────────────────────────────────────────────

fn parse_key(s: &str) -> (u16, u64) {
    match s.to_lowercase().as_str() {
        "fn" | "function" => (VK_FUNCTION, FLAG_FUNCTION),
        "alt" | "option" | "left_alt" | "left_option" => (VK_OPTION, FLAG_OPTION),
        "right_alt" | "altgr" | "right_option" | "alt_right" => (VK_RIGHT_OPTION, FLAG_OPTION),
        "ctrl" | "control" | "left_ctrl" | "left_control" => (VK_CONTROL, FLAG_CONTROL),
        "right_ctrl" | "right_control" => (VK_RIGHT_CONTROL, FLAG_CONTROL),
        _ => {
            log::warn!("Unrecognized key '{s}' — falling back to Alt. \
                        Valid: fn, alt, right_alt, ctrl, right_ctrl");
            (VK_OPTION, FLAG_OPTION)
        }
    }
}

// ── Shared state ─────────────────────────────────────────────────────────────

struct Inner {
    held: bool,
    session_active: bool,
    ptt_mode: bool,
    hold_cancel: Option<Arc<AtomicBool>>,
    target_key_code: u16,
    target_flag_mask: u64,
    on_start: Arc<dyn Fn() + Send + Sync>,
    on_stop: Arc<dyn Fn() + Send + Sync>,
    on_ptt_mode: Arc<dyn Fn() + Send + Sync>,
    /// Tap reference kept for re-enabling after OS invalidation.
    tap_ref: *mut c_void,
}

// Safety: tap_ref is only accessed from the CGEventTap callback thread.
// All other fields are behind Mutex<Inner>.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    fn cancel_timer(&mut self) {
        if let Some(flag) = self.hold_cancel.take() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

enum Action {
    Start {
        cancel: Arc<AtomicBool>,
        on_start: Arc<dyn Fn() + Send + Sync>,
        on_ptt: Arc<dyn Fn() + Send + Sync>,
        /// *mut Mutex<Inner> as usize — safe to send across threads since
        /// the Box is leaked for the lifetime of the process.
        state_ptr: usize,
    },
    Stop(Arc<dyn Fn() + Send + Sync>),
    Nothing,
}

// ── CGEventTap callback ──────────────────────────────────────────────────────

extern "C" fn tap_callback(
    _proxy: *mut c_void,
    event_type: u32,
    event: *mut c_void,
    user_info: *mut c_void,
) -> *mut c_void {
    if user_info.is_null() {
        return event;
    }

    // OS disabled the tap (too slow, or Accessibility revoked) — re-enable.
    if event_type == CG_TAP_DISABLED_BY_TIMEOUT || event_type == CG_TAP_DISABLED_BY_USER_INPUT {
        let state = unsafe { &*(user_info as *const Mutex<Inner>) };
        if let Ok(s) = state.try_lock() {
            if !s.tap_ref.is_null() {
                unsafe { CGEventTapEnable(s.tap_ref, true) };
            }
        }
        log::warn!("CGEventTap disabled by OS (type {event_type:#x}), re-enabled");
        return std::ptr::null_mut();
    }

    if event_type != CG_EVENT_FLAGS_CHANGED || event.is_null() {
        return event;
    }

    let state = unsafe { &*(user_info as *const Mutex<Inner>) };

    let key_code =
        unsafe { CGEventGetIntegerValueField(event, CG_KEYBOARD_EVENT_KEYCODE) as u16 };
    let flags = unsafe { CGEventGetFlags(event) };

    let action = {
        let mut s = match state.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };

        if key_code != s.target_key_code {
            return event;
        }

        let pressed = flags & s.target_flag_mask != 0;

        if pressed {
            if s.held {
                return event; // OS modifier key-repeat equivalent
            }
            s.held = true;

            if s.session_active && !s.ptt_mode {
                // Second tap in toggle mode — VAD or Done button handles stop.
                Action::Nothing
            } else if !s.session_active {
                // Fresh press → start recording + arm hold timer
                s.session_active = true;
                s.ptt_mode = false;
                let cancel = Arc::new(AtomicBool::new(false));
                s.hold_cancel = Some(Arc::clone(&cancel));
                Action::Start {
                    cancel,
                    on_start: Arc::clone(&s.on_start),
                    on_ptt: Arc::clone(&s.on_ptt_mode),
                    state_ptr: user_info as usize,
                }
            } else {
                Action::Nothing
            }
        } else {
            if !s.held {
                return event;
            }
            s.held = false;
            s.cancel_timer();

            if s.session_active && s.ptt_mode {
                // Push-to-talk release → stop
                s.session_active = false;
                s.ptt_mode = false;
                Action::Stop(Arc::clone(&s.on_stop))
            } else {
                // Toggle mode release — keep recording, wait for next press
                Action::Nothing
            }
        }
    }; // lock released

    match action {
        Action::Start { cancel, on_start, on_ptt, state_ptr } => {
            on_start();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(HOLD_THRESHOLD_MS));
                if !cancel.load(Ordering::SeqCst) {
                    let state = unsafe { &*(state_ptr as *const Mutex<Inner>) };
                    let mut s = match state.lock() {
                        Ok(g) => g,
                        Err(e) => e.into_inner(),
                    };
                    if s.session_active && s.held {
                        s.ptt_mode = true;
                        log::debug!("PTT: upgraded to push-to-talk mode");
                        drop(s);
                        on_ptt();
                    }
                }
            });
        }
        Action::Stop(on_stop) => on_stop(),
        Action::Nothing => {}
    }

    event
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Spawns a background thread that creates a CGEventTap and runs a CFRunLoop.
///
/// `on_start`    — called when recording should begin.
/// `on_stop`     — called when recording should end.
/// `on_ptt_mode` — called when hold threshold is reached (PTT mode confirmed).
pub fn start_listener<P, R, M>(key_str: &str, on_start: P, on_stop: R, on_ptt_mode: M)
where
    P: Fn() + Send + Sync + 'static,
    R: Fn() + Send + Sync + 'static,
    M: Fn() + Send + Sync + 'static,
{
    let (target_key_code, target_flag_mask) = parse_key(key_str);

    std::thread::spawn(move || {
        let inner = Inner {
            held: false,
            session_active: false,
            ptt_mode: false,
            hold_cancel: None,
            target_key_code,
            target_flag_mask,
            on_start: Arc::new(on_start),
            on_stop: Arc::new(on_stop),
            on_ptt_mode: Arc::new(on_ptt_mode),
            tap_ref: std::ptr::null_mut(),
        };

        // Leak the Box so the pointer is valid for the entire process lifetime.
        let state: &'static Mutex<Inner> = Box::leak(Box::new(Mutex::new(inner)));
        let state_ptr = state as *const Mutex<Inner> as *mut c_void;

        let tap = unsafe {
            CGEventTapCreate(
                CG_SESSION_EVENT_TAP,
                CG_HEAD_INSERT_EVENT_TAP,
                CG_EVENT_TAP_OPTION_LISTEN_ONLY,
                CG_MASK_FLAGS_CHANGED,
                tap_callback,
                state_ptr,
            )
        };

        if tap.is_null() {
            log::error!(
                "CGEventTapCreate returned null — grant Accessibility permission in \
                 System Settings → Privacy & Security → Accessibility"
            );
            return;
        }

        // Store tap ref so the callback can re-enable it on invalidation.
        if let Ok(mut s) = state.lock() {
            s.tap_ref = tap;
        }

        let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0) };
        let rl = unsafe { CFRunLoopGetCurrent() };
        unsafe {
            CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
            CFRunLoopRun(); // blocks this thread indefinitely
        }

        log::error!("CGEventTap run loop exited unexpectedly — hotkey monitoring stopped");
    });
}
