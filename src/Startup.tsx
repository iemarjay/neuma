import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { motion, AnimatePresence } from "framer-motion";

type ModelStatus = "loading" | "ready" | "missing";
type CleanupMode = "disabled" | "local" | "cloud";

const CLEANUP_LABELS: Record<CleanupMode, string> = {
  disabled: "Off",
  local: "Local",
  cloud: "Cloud",
};

export default function Startup() {
  const [whisper, setWhisper] = useState<ModelStatus>("loading");
  // null = local mode not enabled; otherwise tracks LLM load state
  const [llm, setLlm] = useState<ModelStatus | null>(null);
  const [cleanupMode, setCleanupMode] = useState<CleanupMode | null>(null);
  const [version, setVersion] = useState("");

  const allReady = whisper === "ready" && (llm === null || llm === "ready");
  const anyMissing = whisper === "missing" || llm === "missing";

  useEffect(() => {
    document.body.classList.add("startup-window");
    invoke<string>("get_app_version").then(setVersion);

    // Determine whether local cleanup is enabled before anything else so the
    // LLM row appears immediately rather than popping in after a second fetch.
    invoke<{ cleanup_mode: string }>("get_settings").then((s) => {
      const mode = s.cleanup_mode as CleanupMode;
      setCleanupMode(mode);
      if (mode === "local") {
        setLlm("loading");
        invoke<{ downloaded: boolean }>("get_llm_model_status").then((l) => {
          if (!l.downloaded) setLlm("missing");
        });
      }
    });

    // Whisper — poll current state immediately to catch an already-loaded model.
    invoke<{ downloaded: boolean; loaded: boolean }>("get_model_status").then((s) => {
      if (!s.downloaded) setWhisper("missing");
      else if (s.loaded) setWhisper("ready");
      // else: downloaded but still loading → wait for neuma://model-ready
    });

    const unlistenWhisper = listen("neuma://model-ready", () => setWhisper("ready"));
    const unlistenLlm = listen("neuma://llm-model-ready", () => setLlm("ready"));

    return () => {
      unlistenWhisper.then((fn) => fn());
      unlistenLlm.then((fn) => fn());
    };
  }, []);

  // Auto-hide once everything needed is ready.
  useEffect(() => {
    if (allReady) {
      const t = setTimeout(() => getCurrentWebviewWindow().hide(), 2900);
      return () => clearTimeout(t);
    }
  }, [allReady]);

  const multiRow = llm !== null;

  return (
    <div className="startup-root">
      <motion.div
        className="startup-card"
        initial={{ opacity: 0, scale: 0.96 }}
        animate={{ opacity: 1, scale: 1 }}
        transition={{ duration: 0.28, ease: "easeOut" }}
      >
        <div className="startup-logo">neuma</div>
        <div className="startup-tagline">offline voice dictation</div>

        <div className="startup-status-area">
          {multiRow ? (
            // Two-row layout for local cleanup mode
            <div className="startup-model-rows">
              <ModelRow label="Whisper Turbo" status={whisper} />
              <ModelRow label="Qwen 2.5 1.5B" status={llm!} />
              <AnimatePresence>
                {anyMissing && (
                  <motion.div
                    className="startup-missing-row"
                    initial={{ opacity: 0, y: 4 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={{ opacity: 0 }}
                    transition={{ duration: 0.2 }}
                  >
                    <button
                      type="button"
                      className="startup-btn"
                      onClick={() => {
                        invoke("open_settings_window");
                        getCurrentWebviewWindow().hide();
                      }}
                    >
                      Open Settings →
                    </button>
                  </motion.div>
                )}
              </AnimatePresence>
            </div>
          ) : (
            // Single-row layout for disabled / cloud cleanup modes
            <AnimatePresence mode="wait">
              {whisper === "loading" && (
                <motion.div
                  key="loading"
                  className="startup-status"
                  initial={{ opacity: 0 }}
                  animate={{ opacity: 1 }}
                  exit={{ opacity: 0 }}
                  transition={{ duration: 0.2 }}
                >
                  <div className="startup-dots">
                    <span /><span /><span />
                  </div>
                  <span className="startup-status-text">Loading model</span>
                </motion.div>
              )}
              {whisper === "ready" && (
                <motion.div
                  key="ready"
                  className="startup-status"
                  initial={{ opacity: 0, y: 4 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0 }}
                  transition={{ duration: 0.22 }}
                >
                  <Checkmark />
                  <span className="startup-status-text startup-status-text--ready">Ready</span>
                </motion.div>
              )}
              {whisper === "missing" && (
                <motion.div
                  key="missing"
                  className="startup-status startup-status--column"
                  initial={{ opacity: 0 }}
                  animate={{ opacity: 1 }}
                  exit={{ opacity: 0 }}
                  transition={{ duration: 0.2 }}
                >
                  <span className="startup-status-text">Model not downloaded</span>
                  <button
                    type="button"
                    className="startup-btn"
                    onClick={() => {
                      invoke("open_settings_window");
                      getCurrentWebviewWindow().hide();
                    }}
                  >
                    Open Settings →
                  </button>
                </motion.div>
              )}
            </AnimatePresence>
          )}
        </div>

        <div className="startup-meta">
          <div className="startup-meta-row">
            <span className="startup-meta-label">Text Cleanup</span>
            <span className="startup-meta-value">{cleanupMode ? CLEANUP_LABELS[cleanupMode] : ""}</span>
          </div>
          {version && <span className="startup-meta-version">v{version}</span>}
        </div>
      </motion.div>
    </div>
  );
}

function ModelRow({ label, status }: { label: string; status: ModelStatus }) {
  return (
    <div className="startup-model-row">
      <span className="startup-model-label">{label}</span>
      <AnimatePresence mode="wait">
        {status === "loading" && (
          <motion.span
            key="loading"
            className="startup-model-state"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
          >
            <div className="startup-dots startup-dots--sm">
              <span /><span /><span />
            </div>
          </motion.span>
        )}
        {status === "ready" && (
          <motion.span
            key="ready"
            className="startup-model-state startup-model-state--ready"
            initial={{ opacity: 0, scale: 0.8 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.18 }}
          >
            <Checkmark />
          </motion.span>
        )}
        {status === "missing" && (
          <motion.span
            key="missing"
            className="startup-model-state startup-model-state--missing"
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.15 }}
          >
            Not downloaded
          </motion.span>
        )}
      </AnimatePresence>
    </div>
  );
}

function Checkmark() {
  return (
    <svg width="13" height="13" viewBox="0 0 13 13" fill="none">
      <motion.path
        d="M2 6.5l3 3 6-6"
        stroke="rgba(48,209,88,0.9)"
        strokeWidth="1.8"
        strokeLinecap="round"
        strokeLinejoin="round"
        initial={{ pathLength: 0 }}
        animate={{ pathLength: 1 }}
        transition={{ duration: 0.28, ease: "easeOut" }}
      />
    </svg>
  );
}
