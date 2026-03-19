#![allow(unexpected_cfgs)]

mod audio;
mod ax;
mod cleanup;
mod commands;
mod downloader;
#[cfg(target_os = "macos")]
mod hotkey_listener;
mod local_cleanup;
mod pipeline;
mod settings;
mod transcribe;
mod tray;
mod typer;
mod window;

use audio::AudioRecorder;
use cleanup::CleanupClient;
use local_cleanup::LlmCleanupModel;
use settings::Settings;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    cleanup_client: Arc<CleanupClient>,
    download_cancel: Arc<AtomicBool>,
    llm_download_cancel: Arc<AtomicBool>,
    /// Text before the cursor captured at recording start, used for context-aware cleanup.
    context: Option<String>,
}

impl AppState {
    fn new(settings: Settings) -> Self {
        Self {
            settings,
            recorder: None,
            cleanup_client: Arc::new(CleanupClient::new()),
            download_cancel: Arc::new(AtomicBool::new(false)),
            llm_download_cancel: Arc::new(AtomicBool::new(false)),
            context: None,
        }
    }
}

type SharedState = Arc<Mutex<AppState>>;
type SharedWhisper = Arc<Mutex<Option<Arc<WhisperModel>>>>;
type WhisperLoading = Arc<AtomicBool>;
type SharedLlm = Arc<Mutex<Option<Arc<LlmCleanupModel>>>>;

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
            commands::get_llm_model_status,
            commands::download_llm_model,
            commands::cancel_llm_model_download,
            commands::test_cleanup_connection,
            commands::open_settings_window,
            commands::stop_recording_and_transcribe,
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

    // ── LLM cleanup model ─────────────────────────────────────────────────
    let shared_llm: SharedLlm = Arc::new(Mutex::new(None));
    app.manage(shared_llm.clone());

    let llm_missing = settings.cleanup_mode == "local" && downloader::find_llm_model(app.handle()).is_none();
    if settings.cleanup_mode == "local" {
        if let Some(llm_path) = downloader::find_llm_model(app.handle()) {
            let l = Arc::clone(&shared_llm);
            let app_llm = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                log::info!("loading LLM cleanup model from {llm_path:?}");
                match tokio::task::spawn_blocking(move || LlmCleanupModel::load(&llm_path)).await {
                    Ok(Ok(m)) => {
                        *l.lock().unwrap() = Some(Arc::new(m));
                        log::info!("LLM ready");
                        let _ = app_llm.emit("neuma://llm-model-ready", ());
                    }
                    Ok(Err(e)) => log::error!("LLM load error: {e:#}"),
                    Err(e) => log::error!("LLM load panicked: {e}"),
                }
            });
        } else {
            log::warn!("LLM cleanup model not found — open Settings to download it");
        }
    }

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
    let ax_trusted = window::ax_is_process_trusted();
    let tray_state = tray::build_tray(app.handle(), Arc::clone(&state), ax_trusted)?;
    app.manage(Mutex::new(tray_state));

    if !ax_trusted {
        let _ = app
            .notification()
            .builder()
            .title("Neuma — Action Required")
            .body("Grant Accessibility access so Neuma can detect your hotkey. Open the tray menu to continue.")
            .show();
    }

    // ── Hotkey listener ───────────────────────────────────────────────────
    #[cfg(target_os = "macos")]
    {
        let app_press = app.handle().clone();
        let state_press = Arc::clone(&state);
        let whisper_press = Arc::clone(&shared_whisper);
        let loading_press = Arc::clone(&whisper_loading);
        let llm_press = Arc::clone(&shared_llm);
        let app_release = app.handle().clone();
        let state_release = Arc::clone(&state);
        let whisper_release = Arc::clone(&shared_whisper);
        let loading_release = Arc::clone(&whisper_loading);
        let llm_release = Arc::clone(&shared_llm);
        let app_ptt = app.handle().clone();

        hotkey_listener::start_listener(
            &settings.hotkey,
            move || pipeline::on_hotkey_press(
                app_press.clone(),
                Arc::clone(&state_press),
                Arc::clone(&whisper_press),
                Arc::clone(&loading_press),
                Arc::clone(&llm_press),
            ),
            move || pipeline::on_hotkey_release(
                app_release.clone(),
                Arc::clone(&state_release),
                Arc::clone(&whisper_release),
                Arc::clone(&loading_release),
                Arc::clone(&llm_release),
            ),
            move || window::emit_state(&app_ptt, NeumaState::Listening { mode: ListenMode::Ptt }),
        );
        log::info!("hotkey listener started for key: {}", settings.hotkey);
    }

    // ── Hide from Dock ────────────────────────────────────────────────────
    #[cfg(target_os = "macos")]
    app.set_activation_policy(tauri::ActivationPolicy::Accessory);

    // ── First-run: open Settings if any required model is missing ─────────
    // If a model is missing the startup window will show the download UI — no
    // need to auto-open Settings here.
    if whisper_missing || llm_missing {
        let body = match (whisper_missing, llm_missing) {
            (true, true) => "Whisper and the local AI model need to be downloaded. Open Settings from the startup window to get started.",
            (true, false) => "The Whisper transcription model needs to be downloaded. Open Settings from the startup window to get started.",
            (false, true) => "The local AI cleanup model needs to be downloaded. Open Settings to get started.",
            _ => unreachable!(),
        };
        let _ = app
            .notification()
            .builder()
            .title("Neuma — Download Required")
            .body(body)
            .show();
    }

    Ok(())
}
