use anyhow::Result;
use std::path::Path;

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Context, Result};
    use llama_cpp_2::{
        context::params::LlamaContextParams,
        llama_backend::LlamaBackend,
        llama_batch::LlamaBatch,
        model::{params::LlamaModelParams, AddBos, LlamaModel},
        sampling::LlamaSampler,
        token::LlamaToken,
    };
    use std::num::NonZeroU32;
    use std::path::Path;

    const MAX_OUTPUT_TOKENS: usize = 400;
    const N_CTX: u32 = 2048;

    pub struct LlmCleanupModel {
        model: LlamaModel,
        backend: LlamaBackend,
    }

    impl LlmCleanupModel {
        pub fn load(path: &Path) -> Result<Self> {
            let backend = LlamaBackend::init().context("failed to init llama backend")?;
            let model_params = LlamaModelParams::default().with_n_gpu_layers(99);
            let model = LlamaModel::load_from_file(&backend, path, &model_params)
                .with_context(|| format!("llama: failed to load model from {path:?}"))?;
            Ok(Self { backend, model })
        }

        pub fn clean(&self, text: &str, context: Option<&str>) -> Result<String> {
            let context_section = match context {
                Some(ctx) if !ctx.trim().is_empty() => format!(
                    "\n- If any names or terms in the following document context match \
phonetically with words in the transcript, use their exact spelling:\n\
Document context (text before cursor):\n{}\n",
                    ctx.trim()
                ),
                _ => String::new(),
            };

            let prompt = format!(
                "<|begin_of_text|><|start_header_id|>system<|end_header_id|>\n\n\
You are a voice dictation cleanup engine. Transform raw speech transcription into clean, natural written text.\n\
\n\
Rules:\n\
- Remove filler words (um, uh, like, you know, basically, literally)\n\
- Fix punctuation and capitalization naturally\n\
- Convert spoken list cues (\"one... two...\" or \"first... second...\") into a newline-separated list using \"- \" bullets\n\
- Convert \"new line\" or \"new paragraph\" into actual line breaks\n\
- Convert spoken punctuation (\"exclamation point\", \"question mark\", \"comma\", \"period\") into the actual symbol\n\
- Do not add, infer, or expand on anything not spoken\
{context_section}\
- Output only the cleaned text, nothing else<|eot_id|>\
<|start_header_id|>user<|end_header_id|>\n\n\
{text}<|eot_id|>\
<|start_header_id|>assistant<|end_header_id|>\n\n"
            );

            let tokens: Vec<LlamaToken> = self
                .model
                .str_to_token(&prompt, AddBos::Never)
                .context("failed to tokenise prompt")?;

            let n_prompt = tokens.len();

            let budget = N_CTX as usize;
            if n_prompt + MAX_OUTPUT_TOKENS > budget {
                log::warn!(
                    "cleanup skipped: prompt ({} tokens) + output budget ({}) exceeds n_ctx ({}). \
                     Returning raw transcript.",
                    n_prompt,
                    MAX_OUTPUT_TOKENS,
                    budget
                );
                return Ok(text.to_string());
            }

            let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(N_CTX));
            let mut ctx = self
                .model
                .new_context(&self.backend, ctx_params)
                .context("failed to create llama context")?;

            let mut batch = LlamaBatch::new(N_CTX as usize, 1);
            for (i, &token) in tokens.iter().enumerate() {
                batch
                    .add(token, i as i32, &[0], i == n_prompt - 1)
                    .context("failed to add token to batch")?;
            }
            ctx.decode(&mut batch).context("llama decode failed")?;

            let mut sampler = LlamaSampler::chain_simple([LlamaSampler::greedy()]);
            let mut output = String::new();
            let mut n_pos = n_prompt as i32;
            let mut decoder = encoding_rs::UTF_8.new_decoder();

            loop {
                let token = sampler.sample(&ctx, batch.n_tokens() - 1);
                sampler.accept(token);

                if self.model.is_eog_token(token) {
                    break;
                }

                let piece = self
                    .model
                    .token_to_piece(token, &mut decoder, true, None)
                    .unwrap_or_default();

                if piece.contains("<|eot_id|>") {
                    break;
                }

                output.push_str(&piece);

                let generated = n_pos - n_prompt as i32;
                if generated >= MAX_OUTPUT_TOKENS as i32 {
                    if let Some(pos) = output.rfind(['.', '!', '?', '\n']) {
                        output.truncate(pos + 1);
                    }
                    break;
                }

                batch.clear();
                batch
                    .add(token, n_pos, &[0], true)
                    .context("failed to add generated token to batch")?;
                ctx.decode(&mut batch).context("llama decode failed")?;
                n_pos += 1;
            }

            Ok(output.trim().to_string())
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::LlmCleanupModel;

/// Stub for non-macOS platforms. Local LLM cleanup uses Metal GPU and
/// llama-cpp-2, which conflicts with whisper-rs's bundled ggml on Windows/Linux.
/// On those platforms the cleanup_mode is effectively "disabled" or "cloud".
#[cfg(not(target_os = "macos"))]
pub struct LlmCleanupModel;

#[cfg(not(target_os = "macos"))]
impl LlmCleanupModel {
    pub fn load(_path: &Path) -> Result<Self> {
        anyhow::bail!("local LLM cleanup is not supported on this platform")
    }

    pub fn clean(&self, _text: &str, _context: Option<&str>) -> Result<String> {
        anyhow::bail!("local LLM cleanup is not supported on this platform")
    }
}
