use anyhow::{Context, Result};
use std::path::Path;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Wraps a loaded Whisper model. Cheaply cloneable via Arc inside WhisperContext.
pub struct WhisperModel {
    ctx: WhisperContext,
}

impl WhisperModel {
    /// Load a GGUF/GGML Whisper model from `path`.
    ///
    /// This is slow (~seconds depending on model size). Call once at startup
    /// and keep the result alive for the duration of the app.
    pub fn load(path: &Path) -> Result<Self> {
        let path_str = path
            .to_str()
            .with_context(|| format!("model path is not valid UTF-8: {path:?}"))?;

        let ctx = WhisperContext::new_with_params(path_str, WhisperContextParameters::default())
            .with_context(|| format!("failed to load Whisper model from {path_str}"))?;

        Ok(Self { ctx })
    }

    /// Transcribe 16 kHz mono f32 PCM to text.
    ///
    /// Blocks the calling thread — run inside `tokio::task::spawn_blocking`.
    pub fn transcribe(&self, pcm: &[f32]) -> Result<String> {
        let mut state = self
            .ctx
            .create_state()
            .context("failed to create Whisper state")?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("auto"));
        params.set_translate(false);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_suppress_non_speech_tokens(true);
        params.set_no_context(true); // don't carry context across calls — each dictation is independent
        // Bias Whisper toward clean, punctuated output and away from filler tokens
        params.set_initial_prompt(
            "Transcription of clear spoken English. Proper punctuation and capitalization. \
             No filler words.",
        );
        // Single-threaded — Tauri already manages threads via Tokio
        params.set_n_threads(1);

        state
            .full(params, pcm)
            .context("Whisper transcription failed")?;

        let num_segments = state
            .full_n_segments()
            .context("failed to get segment count")?;

        let mut text = String::new();
        for i in 0..num_segments {
            let segment = state
                .full_get_segment_text(i)
                .with_context(|| format!("failed to get segment {i} text"))?;
            // Whisper sometimes prepends a space; trim and drop hallucinated tokens
            let trimmed = segment.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("[BLANK_AUDIO]") {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(trimmed);
        }

        Ok(text)
    }
}
