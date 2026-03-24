#![allow(unexpected_cfgs)]

mod audio;
mod ax;
mod cleanup;
mod commands;
mod downloader;
#[cfg(target_os = "macos")]
mod hotkey_listener;
mod pipeline;
mod settings;
mod transcribe;
mod tray;
mod typer;
mod window;

use audio::AudioRecorder;
use cleanup::CleanupClient;
use settings::Settings;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_store::StoreExt;
use transcribe::WhisperModel;

// ─── Shared types ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
enum ListenMode {
    Toggle,
    Ptt,
}

#[derive(serde::Serialize, Clone)]
#[serde(tag = "state", rename_all = "camelCase")]
#[allow(dead_code)]
enum NeumaState {
    Idle,
    Loading,
    Listening { mode: ListenMode },
    Transcribing,
    Cleaning,
    Done,
    Error { message: String },
}

#[derive(serde::Serialize, Clone)]
struct AudioLevelPayload {
    level: f32,
}

struct AppState {
    settings: Settings,
    recorder: Option<AudioRecorder>,
    cleanup_client: Option<Arc<CleanupClient>>,
    download_cancel: Arc<AtomicBool>,
    /// Text before the cursor captured at recording start, used for context-aware cleanup.
    context: Option<String>,
}

impl AppState {
    fn new(settings: Settings) -> Self {
        let cleanup_client = CleanupClient::from_settings(
            &settings.cleanup_mode,
            &settings.ollama_url,
            &settings.ollama_model,
        )
        .map(Arc::new);
        Self {
            settings,
            recorder: None,
            cleanup_client,
            download_cancel: Arc::new(AtomicBool::new(false)),
            context: None,
        }
    }

    pub(crate) fn rebuild_cleanup_client(&mut self) {
        self.cleanup_client = CleanupClient::from_settings(
            &self.settings.cleanup_mode,
            &self.settings.ollama_url,
            &self.settings.ollama_model,
        )
        .map(Arc::new);
    }
}

type SharedState = Arc<Mutex<AppState>>;
type SharedWhisper = Arc<Mutex<Option<Arc<WhisperModel>>>>;
type WhisperLoading = Arc<AtomicBool>;

// Prevents Cmd+Q from quitting — only "Quit Neuma" in the tray is honoured.
struct ExplicitQuit(AtomicBool);

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn run() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("panic: {info}");
        log::error!("panic: {info}");
    }));

    tauri::Builder::default()
        .plugin(tauri_plugin_log::Builder::new().level(log::LevelFilter::Info).build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_store::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(setup)
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_settings,
            commands::get_app_version,
            commands::cancel_recording,
            commands::get_model_status,
            commands::download_model,
            commands::cancel_model_download,
            commands::get_ollama_status,
            commands::test_cleanup_connection,
            commands::open_settings_window,
            commands::stop_recording_and_transcribe,
            commands::check_permissions,
            commands::request_permissions,
            commands::check_mic_permission,
            commands::open_microphone_settings,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Neuma")
        .run(|app, event| {
            match event {
                tauri::RunEvent::Ready => {
                    // Pin startup and overlay to all Spaces now that the run loop is active.
                    // setCollectionBehavior called during setup() (before the run loop starts)
                    // is silently ignored by AppKit — must be applied here.
                    #[cfg(target_os = "macos")]
                    for label in ["startup", "overlay"] {
                        if let Some(win) = app.get_webview_window(label) {
                            window::pin_to_all_spaces(&win);
                        }
                    }
                }
                tauri::RunEvent::ExitRequested { api, .. } => {
                    let explicit = app
                        .try_state::<ExplicitQuit>()
                        .map(|s| s.0.load(Ordering::Relaxed))
                        .unwrap_or(false);
                    if !explicit {
                        api.prevent_exit();
                    }
                }
                _ => {}
            }
        });
}

// ─── Setup ────────────────────────────────────────────────────────────────────

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // ── Settings ──────────────────────────────────────────────────────────
    let mut settings = Settings::default();
    if let Ok(store) = app.store("settings.json") {
        if let Some(v) = store.get("settings") {
            if let Ok(s) = serde_json::from_value::<Settings>(v) {
                settings = s;
            }
        }
    }

    let state: SharedState = Arc::new(Mutex::new(AppState::new(settings.clone())));
    app.manage(state.clone());
    app.manage(ExplicitQuit(AtomicBool::new(false)));

    // ── Whisper model ─────────────────────────────────────────────────────
    let shared_whisper: SharedWhisper = Arc::new(Mutex::new(None));
    let whisper_loading: WhisperLoading = Arc::new(AtomicBool::new(false));
    app.manage(shared_whisper.clone());
    app.manage(Arc::clone(&whisper_loading));

    let whisper_missing = downloader::find_model(app.handle(), None).is_none();
    if !whisper_missing {
        let model_path = downloader::find_model(app.handle(), None).unwrap();
        let w = Arc::clone(&shared_whisper);
        let flag = Arc::clone(&whisper_loading);
        let app_model = app.handle().clone();
        flag.store(true, Ordering::Relaxed);
        tauri::async_runtime::spawn(async move {
            log::info!("loading Whisper model from {model_path:?}");
            match tokio::task::spawn_blocking(move || WhisperModel::load(&model_path)).await {
                Ok(Ok(m)) => {
                    *w.lock().unwrap() = Some(Arc::new(m));
                    log::info!("Whisper ready");
                    let _ = app_model.emit("neuma://model-ready", ());
                }
                Ok(Err(e)) => log::error!("Whisper load error: {e}"),
                Err(e) => log::error!("Whisper load panicked: {e}"),
            }
            flag.store(false, Ordering::Relaxed);
        });
    } else {
        log::warn!("Whisper model not found — open Settings to download it");
    }

    // Local LLM cleanup uses Ollama (no in-process model loading needed).

    // ── Windows ───────────────────────────────────────────────────────────
    // Overlay: created hidden — first hotkey press just calls .show(), no latency.
    if let Err(e) = window::create_overlay_window(app.handle()) {
        log::error!("failed to create overlay window: {e}");
    }
    // Startup: visible branded splash, auto-dismissed by frontend on model-ready.
    if let Err(e) = window::create_startup_window(app.handle()) {
        log::error!("failed to create startup window: {e}");
    }

    // ── Tray ─────────────────────────────────────────────────────────────
    let permission_granted = window::listening_permission_granted();
    let tray_state = tray::build_tray(app.handle(), Arc::clone(&state), permission_granted)?;
    app.manage(Mutex::new(tray_state));

    // ── Hotkey listener (conditional on permission) ───────────────────────
    // On macOS 14.2+: needs Input Monitoring (CGPreflightListenEventAccess).
    // On older macOS: needs Accessibility (AXIsProcessTrusted).
    // If already granted, start the tap immediately.
    // If not, poll every second; start the tap once granted — no relaunch needed.
    #[cfg(target_os = "macos")]
    {
        let hotkey = settings.hotkey.clone();

        // Pre-clone all Arcs so they can be moved into either branch.
        let app_press   = app.handle().clone();
        let state_press = Arc::clone(&state);
        let w_press     = Arc::clone(&shared_whisper);
        let l_press     = Arc::clone(&whisper_loading);
        let app_release   = app.handle().clone();
        let state_release = Arc::clone(&state);
        let w_release     = Arc::clone(&shared_whisper);
        let l_release     = Arc::clone(&whisper_loading);
        let app_ptt = app.handle().clone();

        if permission_granted {
            hotkey_listener::start_listener(
                &hotkey,
                move || pipeline::on_hotkey_press(app_press.clone(), Arc::clone(&state_press), Arc::clone(&w_press), Arc::clone(&l_press)),
                move || pipeline::on_hotkey_release(app_release.clone(), Arc::clone(&state_release), Arc::clone(&w_release), Arc::clone(&l_release)),
                move || window::emit_state(&app_ptt, NeumaState::Listening { mode: ListenMode::Ptt }),
            );
            log::info!("hotkey listener started for key: {hotkey}");
        } else {
            let app_h = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let granted = window::listening_permission_granted();
                    let _ = app_h.emit("neuma://permissions", serde_json::json!({ "granted": granted }));
                    if granted {
                        hotkey_listener::start_listener(
                            &hotkey,
                            move || pipeline::on_hotkey_press(app_press.clone(), Arc::clone(&state_press), Arc::clone(&w_press), Arc::clone(&l_press)),
                            move || pipeline::on_hotkey_release(app_release.clone(), Arc::clone(&state_release), Arc::clone(&w_release), Arc::clone(&l_release)),
                            move || window::emit_state(&app_ptt, NeumaState::Listening { mode: ListenMode::Ptt }),
                        );
                        log::info!("hotkey listener started for key: {hotkey}");
                        break;
                    }
                }
            });
        }
    }

    // ── Hide from Dock ────────────────────────────────────────────────────
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    // ── First-run: notify if Whisper model is missing ─────────────────────
    if whisper_missing {
        let _ = app
            .notification()
            .builder()
            .title("Neuma — Download Required")
            .body("The Whisper transcription model needs to be downloaded. Open Settings from the startup window to get started.")
            .show();
    }

    Ok(())
}
