use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const CLEANUP_WORKER_URL: &str = "https://neuma-cleanup.emarjay921.workers.dev";

const OLLAMA_SYSTEM_PROMPT: &str = "\
You are a voice dictation cleanup engine. Transform raw speech transcription into clean, \
natural written text.\n\n\
Rules:\n\
- Remove filler words (um, uh, like, you know, basically, literally)\n\
- Fix punctuation and capitalization naturally\n\
- Convert spoken list cues (\"one... two...\" or \"first... second...\") into a \
newline-separated list using \"- \" bullets\n\
- Convert \"new line\" or \"new paragraph\" into actual line breaks\n\
- Convert spoken punctuation (\"exclamation point\", \"question mark\", \"comma\", \
\"period\") into the actual symbol\n\
- Do not add, infer, or expand on anything not spoken\n\
- Output only the cleaned text, nothing else";

/// Ports probed when the user sets ollama_url to "" or "auto".
const OLLAMA_PROBE_PORTS: &[u16] = &[11434, 11435, 11436];

// ── Shared request/response types ────────────────────────────────────────────

#[derive(Serialize)]
struct CloudRequest<'a> {
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a str>,
}

#[derive(Deserialize)]
struct CloudResponse {
    result: String,
}

// ── Backend mode ──────────────────────────────────────────────────────────────

enum CleanupMode {
    Cloud,
    Local { ollama_url: String, ollama_model: String },
}

// ── Public client ─────────────────────────────────────────────────────────────

/// Unified cleanup client — same `clean()` / `is_available()` interface for
/// both the cloud CF Worker backend and the local Ollama backend.
pub struct CleanupClient {
    client: reqwest::Client,
    mode: CleanupMode,
}

impl CleanupClient {
    pub fn cloud() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("failed to build reqwest client"),
            mode: CleanupMode::Cloud,
        }
    }

    pub fn local(ollama_url: &str, ollama_model: &str) -> Self {
        // "" or "auto" → resolved lazily in is_available() / clean_ollama()
        let resolved_url = match ollama_url.trim() {
            "" | "auto" => String::new(), // will be auto-detected at call time
            u => u.trim_end_matches('/').to_string(),
        };
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("failed to build reqwest client"),
            mode: CleanupMode::Local {
                ollama_url: resolved_url,
                ollama_model: ollama_model.to_string(),
            },
        }
    }

    /// Probe common Ollama ports and return the first reachable base URL,
    /// or `None` if Ollama is not found.
    async fn detect_ollama_url(client: &reqwest::Client) -> Option<String> {
        for &port in OLLAMA_PROBE_PORTS {
            let url = format!("http://localhost:{port}");
            let reachable = client
                .get(format!("{url}/api/tags"))
                .timeout(Duration::from_millis(500))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if reachable {
                log::info!("Ollama auto-detected on port {port}");
                return Some(url);
            }
        }
        None
    }

    /// Build from settings strings. Returns `None` when mode is "disabled".
    pub fn from_settings(mode: &str, ollama_url: &str, ollama_model: &str) -> Option<Self> {
        match mode {
            "cloud" => Some(Self::cloud()),
            "local" => Some(Self::local(ollama_url, ollama_model)),
            _ => None,
        }
    }

    /// Returns true if the backend is reachable.
    pub async fn is_available(&self) -> bool {
        match &self.mode {
            CleanupMode::Cloud => self
                .client
                .head(CLEANUP_WORKER_URL)
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .is_ok(),
            CleanupMode::Local { ollama_url, .. } => {
                let url = if ollama_url.is_empty() {
                    match Self::detect_ollama_url(&self.client).await {
                        Some(u) => u,
                        None => return false,
                    }
                } else {
                    ollama_url.clone()
                };
                self.client
                    .get(format!("{url}/api/tags"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                    .map(|r| r.status().is_success())
                    .unwrap_or(false)
            }
        }
    }

    /// Clean `text` and return the polished result.
    /// For cloud mode `api_key` is required; for local it is ignored.
    pub async fn clean(&self, text: &str, api_key: &str, context: Option<&str>) -> Result<String> {
        match &self.mode {
            CleanupMode::Cloud => self.clean_cloud(text, api_key, context).await,
            CleanupMode::Local { ollama_url, ollama_model } => {
                self.clean_ollama(text, context, ollama_url, ollama_model).await
            }
        }
    }

    /// Ping the cloud worker to verify the API key is valid.
    pub async fn test_connection(&self, api_key: &str) -> Result<()> {
        let url = format!("{CLEANUP_WORKER_URL}/ping");
        let resp = self
            .client
            .post(&url)
            .header("X-API-Key", api_key)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .context("failed to reach cleanup worker")?;
        match resp.status().as_u16() {
            401 | 403 => anyhow::bail!("invalid API key"),
            s if (200..300).contains(&s) => Ok(()),
            s => anyhow::bail!("worker returned {s}"),
        }
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    async fn clean_cloud(&self, text: &str, api_key: &str, context: Option<&str>) -> Result<String> {
        let url = format!("{CLEANUP_WORKER_URL}/cleanup");
        let resp = self
            .client
            .post(&url)
            .header("X-API-Key", api_key)
            .json(&CloudRequest { text, context })
            .send()
            .await
            .with_context(|| format!("failed to POST to cleanup worker at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("cleanup worker returned {status}: {body}");
        }

        let body: CloudResponse = resp.json().await.context("failed to parse cleanup worker response")?;
        Ok(body.result)
    }

    async fn clean_ollama(
        &self,
        text: &str,
        context: Option<&str>,
        ollama_url: &str,
        ollama_model: &str,
    ) -> Result<String> {
        // Resolve URL (auto-detect if empty)
        let ollama_url = if ollama_url.is_empty() {
            Self::detect_ollama_url(&self.client)
                .await
                .ok_or_else(|| anyhow::anyhow!("Ollama not found on ports {:?}", OLLAMA_PROBE_PORTS))?
        } else {
            ollama_url.to_string()
        };
        let ollama_url = &ollama_url;
        let context_section = match context {
            Some(ctx) if !ctx.trim().is_empty() => format!(
                "\n- If any names or terms in the following document context match \
phonetically with words in the transcript, use their exact spelling:\n\
Document context (text before cursor):\n{}\n",
                ctx.trim()
            ),
            _ => String::new(),
        };

        let prompt = format!("{OLLAMA_SYSTEM_PROMPT}{context_section}\n\nTranscript: {text}");

        let resp = self
            .client
            .post(format!("{ollama_url}/api/generate"))
            .json(&serde_json::json!({
                "model": ollama_model,
                "prompt": prompt,
                "stream": false
            }))
            .send()
            .await
            .context("failed to reach Ollama — is it running? (`brew install ollama && ollama serve`)")?;

        if !resp.status().is_success() {
            anyhow::bail!("Ollama returned HTTP {}", resp.status());
        }

        let body: serde_json::Value = resp.json().await.context("invalid JSON from Ollama")?;
        let cleaned = body["response"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no 'response' field in Ollama output"))?
            .trim()
            .to_string();
        Ok(cleaned)
    }
}

impl Default for CleanupClient {
    fn default() -> Self {
        Self::cloud()
    }
}
