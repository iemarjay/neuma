use serde::{Deserialize, Serialize};

fn default_cleanup_mode() -> String {
    "disabled".to_string()
}

fn default_ollama_url() -> String {
    "auto".to_string()
}

fn default_ollama_model() -> String {
    "llama3.2:1b".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// The single key used for both tap-to-toggle and hold-to-push-to-talk.
    /// Valid: "fn", "alt", "right_alt", "ctrl", "right_ctrl".
    pub hotkey: String,
    /// API key for the hosted Neuma cleanup worker (cloud mode only).
    #[serde(default)]
    pub cleanup_api_key: String,
    /// Cleanup mode: "disabled" | "local" | "cloud"
    #[serde(default = "default_cleanup_mode")]
    pub cleanup_mode: String,
    /// Whether to launch Neuma automatically at login.
    #[serde(default)]
    pub launch_at_login: bool,
    /// Ollama base URL for local cleanup (default: http://localhost:11434).
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    /// Ollama model to use for local cleanup (default: llama3.2:1b).
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: "fn".to_string(),
            cleanup_api_key: String::new(),
            cleanup_mode: "disabled".to_string(),
            launch_at_login: false,
            ollama_url: "auto".to_string(),
            ollama_model: default_ollama_model(),
        }
    }
}
