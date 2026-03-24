use std::sync::Arc;
use std::time::Duration;

use audio::AudioRecorder;
use tauri::Manager;

use super::{audio, typer, ListenMode, NeumaState, SharedState, SharedWhisper, WhisperLoading};

use super::window::{
    emit_audio_level, emit_error, emit_state, hide_overlay_after_delay,
    mic_permission_status, open_mic_settings, request_mic_permission,
    position_overlay_bottom_center, show_or_create_settings_window,
};
#[cfg(target_os = "macos")]
use super::window::{set_macos_overlay_level, MicStatus};

/// Shows the overlay, starts recording, and spawns the VAD polling loop.
/// Called after mic permission is confirmed.
fn start_recording(
    app: tauri::AppHandle,
    state: SharedState,
    whisper: SharedWhisper,
    whisper_loading: WhisperLoading,
) {
    // Window ops must run on the main thread on macOS.
    let app_for_window = app.clone();
    app.run_on_main_thread(move || {
        if let Err(e) = position_overlay_bottom_center(&app_for_window) {
            log::warn!("failed to position overlay: {e}");
        }
        if let Some(win) = app_for_window.get_webview_window("overlay") {
            #[cfg(target_os = "macos")]
            set_macos_overlay_level(&win);
            #[cfg(not(target_os = "macos"))]
            let _ = win.show();
        }
    })
    .ok();

    match AudioRecorder::start() {
        Ok(recorder) => {
            #[cfg(target_os = "macos")]
            let context = crate::ax::read_context(500);
            #[cfg(not(target_os = "macos"))]
            let context: Option<String> = None;

            {
                let mut s = state.lock().unwrap();
                s.recorder = Some(recorder);
                s.context = context;
            }
            emit_state(&app, NeumaState::Listening { mode: ListenMode::Toggle });

            let app_clone = app.clone();
            let state_clone = Arc::clone(&state);
            let whisper_clone = Arc::clone(&whisper);
            let loading_clone = Arc::clone(&whisper_loading);

            tauri::async_runtime::spawn(async move {
                loop {
                    let info = {
                        let s = state_clone.lock().unwrap();
                        s.recorder.as_ref().map(|r| r.vad_info())
                    };
                    match info {
                        None => break,
                        Some((level, _silence_progress, vad_stopped)) => {
                            emit_audio_level(&app_clone, level);
                            if vad_stopped {
                                run_pipeline(
                                    app_clone.clone(),
                                    Arc::clone(&state_clone),
                                    Arc::clone(&whisper_clone),
                                    Arc::clone(&loading_clone),
                                );
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
        Err(e) => {
            log::error!("failed to start audio recorder: {e}");
            emit_error(&app, "Microphone not found. Check your audio input device.");
            hide_overlay_after_delay(app, Duration::from_secs(2));
        }
    }
}

/// Shows the startup window with the mic-denied error card.
fn show_mic_denied(app: &tauri::AppHandle) {
    use tauri::Emitter;
    if let Some(win) = app.get_webview_window("startup") {
        let _ = win.show();
    }
    let _ = app.emit("neuma://mic-denied", ());
    open_mic_settings();
}

/// Called when the hotkey is pressed. Checks mic permission before recording.
pub(crate) fn on_hotkey_press(
    app: tauri::AppHandle,
    state: SharedState,
    whisper: SharedWhisper,
    whisper_loading: WhisperLoading,
) {
    // mic_permission_status() is a non-blocking ObjC call — safe on the CGEventTap thread.
    #[cfg(target_os = "macos")]
    match mic_permission_status() {
        MicStatus::Authorized => {
            start_recording(app, state, whisper, whisper_loading);
        }
        MicStatus::NotDetermined => {
            // Request permission asynchronously — must not block the CGEventTap thread.
            let app_c = app.clone();
            let state_c = Arc::clone(&state);
            let whisper_c = Arc::clone(&whisper);
            let loading_c = Arc::clone(&whisper_loading);
            tauri::async_runtime::spawn(async move {
                let granted = tokio::task::spawn_blocking(request_mic_permission)
                    .await
                    .unwrap_or(false);
                if granted {
                    start_recording(app_c, state_c, whisper_c, loading_c);
                } else {
                    emit_error(&app_c, "Microphone access denied.");
                    show_mic_denied(&app_c);
                    hide_overlay_after_delay(app_c, Duration::from_secs(2));
                }
            });
        }
        MicStatus::Denied | MicStatus::Restricted => {
            emit_error(&app, "Microphone access denied. Enable it in System Settings.");
            show_mic_denied(&app);
            hide_overlay_after_delay(app, Duration::from_secs(2));
        }
    }

    // On non-macOS mic permission is always available — record directly.
    #[cfg(not(target_os = "macos"))]
    start_recording(app, state, whisper, whisper_loading);
}


/// Called when the hotkey is released in PTT mode. Kicks off the pipeline.
pub(crate) fn on_hotkey_release(
    app: tauri::AppHandle,
    state: SharedState,
    whisper: SharedWhisper,
    whisper_loading: WhisperLoading,
) {
    run_pipeline(app, state, whisper, whisper_loading);
}

/// Takes the recorder from state and runs the full transcribe → cleanup → inject
/// pipeline. Safe to call from the VAD loop, Done button, or PTT release — the
/// `recorder.take()` is idempotent (returns early if already taken).
pub(crate) fn run_pipeline(
    app: tauri::AppHandle,
    state: SharedState,
    whisper: SharedWhisper,
    whisper_loading: WhisperLoading,
) {
    let recorder = {
        let mut s = state.lock().unwrap();
        s.recorder.take()
    };

    let Some(recorder) = recorder else {
        return;
    };

    let whisper_snap = whisper.lock().unwrap().clone();
    let Some(whisper_model) = whisper_snap else {
        let is_loading = whisper_loading.load(std::sync::atomic::Ordering::Relaxed);
        let msg = if is_loading {
            "Whisper model still loading — wait a moment and try again."
        } else {
            "Whisper model not downloaded. Opening Settings to download it."
        };
        emit_error(&app, msg);
        if !is_loading {
            let _ = show_or_create_settings_window(&app);
        }
        hide_overlay_after_delay(app, Duration::from_secs(2));
        return;
    };

    let (cleanup_api_key, cleanup_client, context) = {
        let mut s = state.lock().unwrap();
        (
            s.settings.cleanup_api_key.clone(),
            s.cleanup_client.clone(),
            s.context.take(),
        )
    };

    emit_state(&app, NeumaState::Transcribing);

    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        // ── Stop recording ────────────────────────────────────────────────
        let pcm = match recorder.stop() {
            Ok(p) => p,
            Err(e) => {
                log::error!("recorder stop failed: {e}");
                emit_error(&app_clone, "Recording stopped unexpectedly.");
                hide_overlay_after_delay(app_clone, Duration::from_secs(2));
                return;
            }
        };

        if pcm.is_empty() {
            emit_state(&app_clone, NeumaState::Error {
                message: "Nothing was captured. Try speaking closer to the mic.".into(),
            });
            hide_overlay_after_delay(app_clone, Duration::from_secs(2));
            return;
        }

        // ── Transcribe ────────────────────────────────────────────────────
        let transcript =
            match tokio::task::spawn_blocking(move || whisper_model.transcribe(&pcm)).await {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    log::error!("transcription failed: {e}");
                    emit_error(&app_clone, "Transcription failed. Is the Whisper model installed?");
                    hide_overlay_after_delay(app_clone, Duration::from_secs(2));
                    return;
                }
                Err(e) => {
                    log::error!("transcription task panicked: {e}");
                    emit_error(&app_clone, "Transcription crashed unexpectedly.");
                    hide_overlay_after_delay(app_clone, Duration::from_secs(2));
                    return;
                }
            };

        if transcript.is_empty() {
            emit_state(&app_clone, NeumaState::Error {
                message: "Nothing was picked up. Speak more clearly or check your mic.".into(),
            });
            hide_overlay_after_delay(app_clone, Duration::from_secs(2));
            return;
        }

        // ── Cleanup ───────────────────────────────────────────────────────
        let final_text = run_cleanup(transcript, cleanup_client, &cleanup_api_key, context, &app_clone).await;

        // ── Inject ────────────────────────────────────────────────────────
        let text_for_inject = final_text.clone();
        match tokio::task::spawn_blocking(move || typer::inject(&text_for_inject)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("text inject failed: {e}");
                emit_error(&app_clone, "Couldn't insert text. Make sure a text field is focused.");
                hide_overlay_after_delay(app_clone, Duration::from_secs(2));
                return;
            }
            Err(e) => {
                log::error!("text inject task panicked: {e}");
                emit_error(&app_clone, "Text injection crashed unexpectedly.");
                hide_overlay_after_delay(app_clone, Duration::from_secs(2));
                return;
            }
        }

        emit_state(&app_clone, NeumaState::Done);
        tokio::time::sleep(Duration::from_millis(2200)).await;
        if let Some(win) = app_clone.get_webview_window("overlay") {
            let _ = win.hide();
        }
        emit_state(&app_clone, NeumaState::Idle);
    });
}

async fn run_cleanup(
    transcript: String,
    client: Option<Arc<super::cleanup::CleanupClient>>,
    api_key: &str,
    context: Option<String>,
    app: &tauri::AppHandle,
) -> String {
    let Some(client) = client else {
        return transcript; // cleanup_mode == "disabled"
    };

    if !client.is_available().await {
        log::info!("cleanup backend unavailable — skipping");
        return transcript;
    }

    emit_state(app, NeumaState::Cleaning);
    match client.clean(&transcript, api_key, context.as_deref()).await {
        Ok(cleaned) => cleaned,
        Err(e) => {
            log::warn!("cleanup failed, using raw transcript: {e}");
            transcript
        }
    }
}
