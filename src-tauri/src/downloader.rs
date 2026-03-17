use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager};
use tokio::fs;
use tokio::io::AsyncWriteExt;

const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin";
const MODEL_FILENAME: &str = "ggml-whisper-turbo.bin";

const LLM_MODEL_URL: &str =
    "https://huggingface.co/bartowski/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/Qwen2.5-1.5B-Instruct-Q4_K_M.gguf";
const LLM_MODEL_FILENAME: &str = "qwen2.5-1.5b-instruct-q4_k_m.gguf";

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgressPayload {
    pub downloaded: u64,
    pub total: u64,
    pub speed_bps: f64,
    pub eta_secs: u64,
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

pub fn model_path(app: &AppHandle) -> PathBuf {
    if let Ok(data_dir) = app.path().app_data_dir() {
        return data_dir.join("models").join(MODEL_FILENAME);
    }
    PathBuf::from("models").join(MODEL_FILENAME)
}

pub fn find_model(app: &AppHandle, custom_path: Option<&str>) -> Option<PathBuf> {
    if let Some(path_str) = custom_path {
        let p = PathBuf::from(path_str);
        if p.is_absolute() {
            if p.exists() {
                return Some(p);
            }
        } else if let Ok(cwd) = std::env::current_dir() {
            let full = cwd.join(path_str);
            if full.exists() {
                return Some(full);
            }
        }
    }

    let primary = model_path(app);
    if primary.exists() {
        return Some(primary);
    }

    // Dev-only: walk up from cwd so `cargo tauri dev` finds models/ in the project root.
    // In release builds the model must be at the canonical app-data path above.
    #[cfg(debug_assertions)]
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        for _ in 0..4 {
            let candidate = dir.join("models").join(MODEL_FILENAME);
            if candidate.exists() {
                return Some(candidate);
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => break,
            }
        }
    }

    None
}

pub fn llm_model_path(app: &AppHandle) -> PathBuf {
    if let Ok(data_dir) = app.path().app_data_dir() {
        return data_dir.join("models").join(LLM_MODEL_FILENAME);
    }
    PathBuf::from("models").join(LLM_MODEL_FILENAME)
}

pub fn find_llm_model(app: &AppHandle) -> Option<PathBuf> {
    let primary = llm_model_path(app);
    if primary.exists() {
        return Some(primary);
    }

    #[cfg(debug_assertions)]
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        for _ in 0..4 {
            let candidate = dir.join("models").join(LLM_MODEL_FILENAME);
            if candidate.exists() {
                return Some(candidate);
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => break,
            }
        }
    }

    None
}

// ─── Generic download ─────────────────────────────────────────────────────────

/// Download `url` to `target`, resuming if a `.part` file exists.
///
/// Emits `{event_prefix}-progress` (~5×/sec) and `{event_prefix}-complete`
/// via the Tauri event system. Errors are returned to the caller, who is
/// responsible for emitting `{event_prefix}-error`.
///
/// After the rename from `.part` → `target` the file size is verified against
/// the server-reported `Content-Length`. A mismatch (e.g. a truncated
/// HuggingFace response) deletes the corrupt file and returns an error rather
/// than leaving a broken model on disk.
async fn download_file(
    app: &AppHandle,
    url: &str,
    target: PathBuf,
    event_prefix: &str,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    // Partial download lives alongside the target with a `.part` suffix appended
    // to the full filename (e.g. `ggml-whisper-turbo.bin.part`).
    let part_path = target.with_file_name(format!(
        "{}.part",
        target
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .context("failed to create models directory")?;
    }

    let already_downloaded = if part_path.exists() {
        fs::metadata(&part_path).await.map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?;

    let mut req = client.get(url);
    if already_downloaded > 0 {
        req = req.header("Range", format!("bytes={}-", already_downloaded));
        log::info!("resuming download of {} from byte {already_downloaded}", target.display());
    }

    let mut resp = req.send().await.context("failed to start download")?;

    if !resp.status().is_success() && resp.status().as_u16() != 206 {
        anyhow::bail!("unexpected HTTP status: {}", resp.status());
    }

    let content_length = resp.content_length().unwrap_or(0);
    let total = if already_downloaded > 0 && content_length > 0 {
        already_downloaded + content_length
    } else {
        content_length
    };

    let mut file = if already_downloaded > 0 {
        fs::OpenOptions::new()
            .append(true)
            .open(&part_path)
            .await
            .context("failed to open partial download file")?
    } else {
        fs::File::create(&part_path)
            .await
            .context("failed to create download file")?
    };

    let mut downloaded = already_downloaded;
    let session_start = Instant::now();
    let mut last_emit = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!("cancelled"));
        }

        match resp.chunk().await.context("error reading download chunk")? {
            None => break,
            Some(chunk) => {
                file.write_all(&chunk)
                    .await
                    .context("failed to write chunk to disk")?;
                downloaded += chunk.len() as u64;

                if last_emit.elapsed().as_millis() >= 200 {
                    let elapsed = session_start.elapsed().as_secs_f64();
                    let session_bytes = downloaded - already_downloaded;
                    let speed_bps = if elapsed > 0.0 {
                        session_bytes as f64 / elapsed
                    } else {
                        0.0
                    };
                    let remaining = total.saturating_sub(downloaded);
                    let eta_secs = if speed_bps > 0.0 {
                        (remaining as f64 / speed_bps) as u64
                    } else {
                        0
                    };

                    let _ = app.emit(
                        &format!("{event_prefix}-progress"),
                        DownloadProgressPayload { downloaded, total, speed_bps, eta_secs },
                    );
                    last_emit = Instant::now();
                }
            }
        }
    }

    file.flush().await?;
    drop(file);

    fs::rename(&part_path, &target)
        .await
        .context("failed to finalize downloaded file")?;

    // Verify the file is complete. HuggingFace occasionally closes the
    // connection early; without this check the truncated file gets loaded
    // and produces a cryptic model-load failure on next launch.
    if total > 0 {
        let actual = fs::metadata(&target).await?.len();
        if actual != total {
            fs::remove_file(&target).await.ok();
            anyhow::bail!(
                "download incomplete: server reported {} bytes but received {}",
                total,
                actual
            );
        }
    }

    log::info!("download complete → {}", target.display());
    let _ = app.emit(&format!("{event_prefix}-complete"), serde_json::json!({}));

    Ok(())
}

// ─── Public callers ───────────────────────────────────────────────────────────

pub async fn download_model(app: AppHandle, cancel: Arc<AtomicBool>) -> Result<()> {
    download_file(&app, MODEL_URL, model_path(&app), "neuma://download", cancel).await
}

pub async fn download_llm_model(app: AppHandle, cancel: Arc<AtomicBool>) -> Result<()> {
    download_file(&app, LLM_MODEL_URL, llm_model_path(&app), "neuma://llm-download", cancel).await
}
