/// Single-key listener with tap-vs-hold mode detection.
///
/// One key, two behaviors decided at release time:
///   Tap  (release < HOLD_THRESHOLD_MS) → toggle mode: recording continues until next tap.
///   Hold (release ≥ HOLD_THRESHOLD_MS) → push-to-talk: recording stops on release.
///
/// Recording starts immediately on every fresh key press regardless of mode.
///
/// Uses rdev (CGEventTap on macOS) — supports fn, standalone Alt/Option, etc.
/// Requires Accessibility permission on macOS:
///   System Settings → Privacy & Security → Accessibility
///
/// Valid `key_str` values
/// ─────────────────────
///   "fn" / "function"                        → Fn key
///   "alt" / "option" / "left_alt"            → Left Option / Left Alt
///   "right_alt" / "altgr" / "right_option"   → Right Option / AltGr
///   "ctrl" / "control" / "left_ctrl"         → Left Control
///   "right_ctrl" / "right_control"           → Right Control

use rdev::{listen, Event, EventType, Key};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

/// Milliseconds a key must be held before it switches to push-to-talk mode.
const HOLD_THRESHOLD_MS: u64 = 400;

// ─── Key mapping ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum PttKey {
    Fn,
    Alt,
    AltRight,
    CtrlLeft,
    CtrlRight,
}

impl PttKey {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "fn" | "function" => Some(Self::Fn),
            "alt" | "option" | "left_alt" | "left_option" => Some(Self::Alt),
            "right_alt" | "altgr" | "right_option" | "alt_right" => Some(Self::AltRight),
            "ctrl" | "control" | "left_ctrl" | "left_control" => Some(Self::CtrlLeft),
            "right_ctrl" | "right_control" => Some(Self::CtrlRight),
            _ => None,
        }
    }

    fn matches(self, key: &Key) -> bool {
        match (self, key) {
            (Self::Fn, Key::Function) => true,
            (Self::Alt, Key::Alt) => true,
            (Self::AltRight, Key::AltGr) => true,
            (Self::CtrlLeft, Key::ControlLeft) => true,
            (Self::CtrlRight, Key::ControlRight) => true,
            _ => false,
        }
    }
}

// ─── Shared listener state ────────────────────────────────────────────────────

struct State {
    /// Is the physical key currently held down?
    held: bool,
    /// Are we in an active recording session started by this key?
    session_active: bool,
    /// Has the current session crossed the hold threshold (PTT mode)?
    ptt_mode: bool,
    /// Flag to cancel the in-flight hold timer. Set to true to cancel.
    hold_cancel: Option<Arc<AtomicBool>>,
}

impl State {
    fn new() -> Self {
        Self {
            held: false,
            session_active: false,
            ptt_mode: false,
            hold_cancel: None,
        }
    }

    fn cancel_timer(&mut self) {
        if let Some(flag) = self.hold_cancel.take() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

// ─── Actions resolved outside the mutex lock ─────────────────────────────────

#[derive(Debug)]
enum Action {
    /// Start a new recording session and arm the hold timer.
    StartAndArmTimer(Arc<AtomicBool>),
    /// Stop the current recording session.
    Stop,
    Nothing,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Spawns a background thread that listens for the configured key.
///
/// `on_start`    — called when recording should begin (key press, no active session).
/// `on_stop`     — called when recording should end (tap: second press; hold: release).
/// `on_ptt_mode` — called when the hold threshold passes and PTT mode is confirmed.
pub fn start_listener<P, R, M>(key_str: &str, on_start: P, on_stop: R, on_ptt_mode: M)
where
    P: Fn() + Send + Sync + 'static,
    R: Fn() + Send + Sync + 'static,
    M: Fn() + Send + Sync + 'static,
{
    let ptt_key = match PttKey::from_str(key_str) {
        Some(k) => k,
        None => {
            log::warn!(
                "Unrecognized PTT key '{key_str}' — falling back to Fn. \
                 Valid: fn, alt, right_alt, ctrl, right_ctrl"
            );
            PttKey::Fn
        }
    };

    let shared = Arc::new(Mutex::new(State::new()));
    let on_start = Arc::new(on_start);
    let on_stop = Arc::new(on_stop);
    let on_ptt_mode = Arc::new(on_ptt_mode);

    // Clone Arcs for the listen closure (the closure is 'static + move)
    let shared_cb = Arc::clone(&shared);
    let on_start_cb = Arc::clone(&on_start);
    let on_stop_cb = Arc::clone(&on_stop);
    let on_ptt_mode_cb = Arc::clone(&on_ptt_mode);

    std::thread::spawn(move || {
        let callback = move |event: Event| {
            let action = match event.event_type {
                EventType::KeyPress(ref key) if ptt_key.matches(key) => {
                    let mut s = shared_cb.lock().unwrap();

                    if s.held {
                        // OS key-repeat — ignore
                        return;
                    }
                    s.held = true;

                    if s.session_active && !s.ptt_mode {
                        // Second tap while in toggle mode → stop
                        s.cancel_timer();
                        s.session_active = false;
                        Action::Stop
                    } else if !s.session_active {
                        // Fresh press → start recording, arm hold timer
                        s.session_active = true;
                        s.ptt_mode = false;
                        let cancel = Arc::new(AtomicBool::new(false));
                        s.hold_cancel = Some(Arc::clone(&cancel));
                        Action::StartAndArmTimer(cancel)
                    } else {
                        Action::Nothing
                    }
                }

                EventType::KeyRelease(ref key) if ptt_key.matches(key) => {
                    let mut s = shared_cb.lock().unwrap();
                    s.held = false;
                    s.cancel_timer();

                    if s.session_active && s.ptt_mode {
                        // Push-to-talk release → stop
                        s.session_active = false;
                        s.ptt_mode = false;
                        Action::Stop
                    } else {
                        // Toggle mode release → keep recording, wait for next press
                        Action::Nothing
                    }
                }

                _ => return,
            };

            // Execute actions with the lock released
            match action {
                Action::StartAndArmTimer(cancel) => {
                    on_start_cb();

                    // Arm hold timer: after threshold, mark session as PTT mode
                    let shared_timer = Arc::clone(&shared_cb);
                    let on_ptt_mode_timer = Arc::clone(&on_ptt_mode_cb);
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(HOLD_THRESHOLD_MS));
                        if !cancel.load(Ordering::SeqCst) {
                            let mut s = shared_timer.lock().unwrap();
                            // Only upgrade if still actively held in a session
                            if s.session_active && s.held {
                                s.ptt_mode = true;
                                log::debug!("PTT: upgraded to push-to-talk mode");
                                drop(s); // release lock before calling callback
                                on_ptt_mode_timer();
                            }
                        }
                    });
                }
                Action::Stop => {
                    on_stop_cb();
                }
                Action::Nothing => {}
            }
        };

        if let Err(e) = listen(callback) {
            log::error!(
                "Key listener exited (grant Accessibility permission in \
                 System Settings → Privacy & Security → Accessibility): {e:?}"
            );
        }
    });
}
