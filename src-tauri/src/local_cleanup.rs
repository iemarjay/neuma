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

/// Maximum tokens the model is allowed to generate per cleanup call.
/// Typical dictation outputs 50–200 tokens. 400 gives ample headroom while
/// staying well within the context window budget.
const MAX_OUTPUT_TOKENS: usize = 400;

/// Context window allocated for the model. Prompt overhead is ~80 tokens,
/// leaving ~1520 tokens for input. At ~0.75 words/token, that covers ~1140
/// words — roughly a 4-minute dictation clip. Inputs beyond this are logged
/// and passed through uncleaned rather than silently truncated mid-sentence.
const N_CTX: u32 = 2048;

pub struct LlmCleanupModel {
    backend: LlamaBackend,
    model: LlamaModel,
}

impl LlmCleanupModel {
    pub fn load(path: &Path) -> Result<Self> {
        let backend = LlamaBackend::init().context("failed to init llama backend")?;
        // Start with CPU-only (n_gpu_layers=0) to rule out Metal backend issues.
        // Once confirmed stable we can raise this to offload layers to GPU.
        let model_params = LlamaModelParams::default().with_n_gpu_layers(0);
        let model = LlamaModel::load_from_file(&backend, path, &model_params)
            .with_context(|| format!("llama: failed to load model from {path:?}"))?;
        Ok(Self { backend, model })
    }

    /// Remove filler words and fix punctuation in transcribed text.
    ///
    /// # Context allocation
    /// A fresh `LlamaContext` (KV cache) is created on every call. This costs
    /// ~20–50ms and ~30 MB of RAM per invocation. For Neuma's use-case —
    /// one cleanup per dictation, not a continuous generation loop — this is
    /// acceptable. Reusing the context across calls would require a
    /// self-referential struct (context borrows model) or an unsafe lifetime
    /// extension, adding significant complexity for marginal gain given typical
    /// dictation frequency (once every several seconds at most).
    pub fn clean(&self, text: &str) -> Result<String> {
        // Qwen2.5-Instruct uses ChatML format
        let prompt = format!(
            "<|im_start|>system\n\
You are a voice dictation cleanup engine. Transform raw speech transcription into clean, natural written text.\n\
\n\
Rules:\n\
- Remove filler words (um, uh, like, you know, basically, literally)\n\
- Remove false starts and self-corrections — keep only the intended version (e.g. \"let's meet at 2... actually 3\" → \"let's meet at 3\")\n\
- Fix punctuation and capitalization naturally\n\
- Preserve the speaker's tone and vocabulary — do not rewrite or rephrase\n\
- Convert spoken list cues (\"one... two...\" or \"first... second...\") into a newline-separated list using \"- \" bullets\n\
- Convert \"new line\" or \"new paragraph\" into actual line breaks\n\
- Convert spoken punctuation (\"exclamation point\", \"question mark\", \"comma\", \"period\") into the actual symbol\n\
- Do not add, infer, or expand on anything not spoken\n\
- Output only the cleaned text, nothing else\
<|im_end|>\n\
<|im_start|>user\n\
{text}\
<|im_end|>\n\
<|im_start|>assistant\n"
        );

        let tokens: Vec<LlamaToken> = self
            .model
            .str_to_token(&prompt, AddBos::Always)
            .context("failed to tokenise prompt")?;

        let n_prompt = tokens.len();

        // Guard against context overflow. The prompt plus max output must fit
        // within N_CTX. If the input is too long, return the raw text rather
        // than crashing or silently truncating mid-sentence.
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

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX));
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

            if piece.contains("<|im_end|>") {
                break;
            }

            output.push_str(&piece);

            // Hard cap: trim to the last sentence boundary so we don't cut
            // mid-word. Accept the output as-is if no boundary is found.
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
