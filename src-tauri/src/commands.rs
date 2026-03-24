use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_store::StoreExt;

use tauri::{Emitter, Manager};

use super::{downloader, NeumaState, SharedState, SharedWhisper, WhisperLoading};
use super::cleanup::CleanupClient;
use super::settings::Settings;
use super::tray::TrayState;
use super::transcribe::WhisperModel;
use super::window::{
    emit_state, hide_overlay_after_delay,
    listening_permission_granted, listening_permission_type, request_listening_permission,
    mic_permission_status, open_mic_settings,
    show_or_create_settings_window,
};

// ─── Settings ─────────────────────────────────────────────────────────────────

#[tauri::command]
pub(crate) fn get_settings(state: tauri::State<SharedState>) -> Settings {
    state.lock().unwrap().settings.clone()
}

#[tauri::command]
pub(crate) fn save_settings(
    new_settings: Settings,
    state: tauri::State<SharedState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let launch_at_login = new_settings.launch_at_login;
    {
        let mut s = state.lock().unwrap();
        s.settings = new_settings.clone();
        s.rebuild_cleanup_client();
    }

    // Sync autostart
    let autolaunch = app.autolaunch();
    let currently_enabled = autolaunch.is_enabled().unwrap_or(false);
    if launch_at_login && !currently_enabled {
        let _ = autolaunch.enable();
    } else if !launch_at_login && currently_enabled {
        let _ = autolaunch.disable();
    }

    // Sync tray checkmark
    if let Some(tray_state) = app.try_state::<Mutex<TrayState>>() {
        if let Ok(ts) = tray_state.lock() {
            let _ = ts.launch_at_login_item.set_checked(launch_at_login);
        }
    }

    // Persist
    if let Ok(store) = app.store("settings.json") {
        let v = serde_json::to_value(&new_settings).map_err(|e| e.to_string())?;
        store.set("settings", v);
        store.save().map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
pub(crate) fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

// ─── Recording ────────────────────────────────────────────────────────────────

#[tauri::command]
pub(crate) fn cancel_recording(state: tauri::State<SharedState>, app: tauri::AppHandle) {
    {
        let mut s = state.lock().unwrap();
        s.recorder = None;
    }
    emit_state(&app, NeumaState::Idle);
    hide_overlay_after_delay(app, Duration::from_millis(300));
}

/// Stop recording and run the full transcribe → cleanup → inject pipeline.
/// Called by the Done (✓) button in the overlay — equivalent to VAD auto-stop.
#[tauri::command]
pub(crate) fn stop_recording_and_transcribe(
    app: tauri::AppHandle,
    state: tauri::State<'_, SharedState>,
) {
    let whisper = app.state::<SharedWhisper>().inner().clone();
    let whisper_loading = app.state::<WhisperLoading>().inner().clone();
    let state = Arc::clone(state.inner());
    super::pipeline::run_pipeline(app, state, whisper, whisper_loading);
}

// ─── Whisper model ────────────────────────────────────────────────────────────

#[tauri::command]
pub(crate) fn get_model_status(
    app: tauri::AppHandle,
    whisper: tauri::State<'_, SharedWhisper>,
) -> serde_json::Value {
    let downloaded = downloader::find_model(&app, None).is_some();
    let loaded = whisper.lock().unwrap().is_some();
    serde_json::json!({ "downloaded": downloaded, "loaded": loaded })
}

#[tauri::command]
pub(crate) async fn download_model(
    app: tauri::AppHandle,
    state: tauri::State<'_, SharedState>,
    whisper: tauri::State<'_, SharedWhisper>,
) -> Result<(), String> {
    let cancel = state.lock().unwrap().download_cancel.clone();
    cancel.store(false, Ordering::Relaxed);

    let app_clone = app.clone();
    let cancel_clone = Arc::clone(&cancel);
    let whisper_clone = Arc::clone(&whisper);

    tauri::async_runtime::spawn(async move {
        match downloader::download_model(app_clone.clone(), cancel_clone).await {
            Ok(()) => {
                let model_path = downloader::model_path(&app_clone);
                tauri::async_runtime::spawn(async move {
                    log::info!("loading Whisper model after download");
                    match tokio::task::spawn_blocking(move || WhisperModel::load(&model_path)).await {
                        Ok(Ok(model)) => {
                            *whisper_clone.lock().unwrap() = Some(Arc::new(model));
                            log::info!("Whisper model loaded after download");
                        }
                        Ok(Err(e)) => log::error!("Whisper load error (post-download): {e}"),
                        Err(e) => log::error!("Whisper load panicked (post-download): {e}"),
                    }
                });
            }
            Err(e) if e.to_string() == "cancelled" => log::info!("Whisper download cancelled"),
            Err(e) => {
                log::error!("Whisper download failed: {e}");
                let _ = app_clone.emit(
                    "neuma://download-error",
                    serde_json::json!({ "message": e.to_string() }),
                );
            }
        }
    });

    Ok(())
}

#[tauri::command]
pub(crate) fn cancel_model_download(state: tauri::State<SharedState>) {
    state.lock().unwrap().download_cancel.store(true, Ordering::Relaxed);
}

// ─── Ollama / local cleanup ───────────────────────────────────────────────────

/// Check whether Ollama is reachable at the configured URL.
#[tauri::command]
pub(crate) async fn get_ollama_status(state: tauri::State<'_, SharedState>) -> Result<serde_json::Value, ()> {
    let client = state.lock().unwrap().cleanup_client.clone();
    let available = match client {
        Some(c) => c.is_available().await,
        None => CleanupClient::local("auto", "").is_available().await,
    };
    Ok(serde_json::json!({ "available": available }))
}

// ─── Misc ─────────────────────────────────────────────────────────────────────

#[tauri::command]
pub(crate) async fn test_cleanup_connection(api_key: String) -> Result<(), String> {
    CleanupClient::cloud()
        .test_connection(&api_key)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn open_settings_window(app: tauri::AppHandle) -> Result<(), String> {
    show_or_create_settings_window(&app).map_err(|e| e.to_string())
}

// ─── Permissions ──────────────────────────────────────────────────────────────

/// Returns whether the one required permission for a listen-only CGEventTap is
/// granted, plus which permission type it is for display purposes.
///
/// On macOS 14.2+: `permission_type` = "input_monitoring" (CGPreflightListenEventAccess).
/// On older macOS: `permission_type` = "accessibility"    (AXIsProcessTrusted).
#[tauri::command]
pub(crate) fn check_permissions() -> serde_json::Value {
    serde_json::json!({
        "granted": listening_permission_granted(),
        "permission_type": listening_permission_type(),
    })
}

/// Requests the permission needed for the hotkey listener and opens the relevant
/// System Settings pane so the user can grant it.
#[tauri::command]
pub(crate) fn request_permissions() {
    request_listening_permission();
}

/// Returns the current microphone authorization status.
/// Used by Startup.tsx to poll for mic grant after a mic-denied error.
#[tauri::command]
pub(crate) fn check_mic_permission() -> super::window::MicStatus {
    mic_permission_status()
}

/// Opens System Settings → Microphone.
/// Called from the mic-denied card in Startup.tsx.
#[tauri::command]
pub(crate) fn open_microphone_settings() {
    open_mic_settings();
}
