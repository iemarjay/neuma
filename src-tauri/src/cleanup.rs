use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Hosted Neuma cleanup worker URL. Not user-configurable — users supply an API key only.
const CLEANUP_WORKER_URL: &str = "https://neuma-cleanup.emarjay921.workers.dev";

#[derive(Serialize)]
struct CleanupRequest<'a> {
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a str>,
}

#[derive(Deserialize)]
struct CleanupResponse {
    result: String,
}

/// HTTP client for the optional CF Worker text cleanup endpoint.
pub struct CleanupClient {
    client: reqwest::Client,
}

impl CleanupClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("failed to build reqwest client");
        Self { client }
    }

    /// Quick connectivity check. Returns true if the cleanup worker is reachable.
    pub async fn is_online(&self) -> bool {
        self.client
            .head(CLEANUP_WORKER_URL)
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .is_ok()
    }

    /// Send `text` to the CF Worker cleanup endpoint and return cleaned text.
    /// `context` is optional text before the cursor for context-aware spelling.
    pub async fn clean(&self, text: &str, api_key: &str, context: Option<&str>) -> Result<String> {
        let url = format!("{}/cleanup", CLEANUP_WORKER_URL);

        let resp = self
            .client
            .post(&url)
            .header("X-API-Key", api_key)
            .json(&CleanupRequest { text, context })
            .send()
            .await
            .with_context(|| format!("failed to POST to cleanup worker at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("cleanup worker returned {status}: {body}");
        }

        let body: CleanupResponse = resp
            .json()
            .await
            .context("failed to parse cleanup worker response")?;

        Ok(body.result)
    }

    /// Ping the worker to verify the API key is valid.
    /// Expects a `/ping` route on the worker that validates the key and returns 200.
    pub async fn test_connection(&self, api_key: &str) -> Result<()> {
        let url = format!("{}/ping", CLEANUP_WORKER_URL);

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
            s if s >= 200 && s < 300 => Ok(()),
            s => anyhow::bail!("worker returned {s}"),
        }
    }
}

impl Default for CleanupClient {
    fn default() -> Self {
        Self::new()
    }
}
