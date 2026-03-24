import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { motion, AnimatePresence } from "framer-motion";

type ModelStatus = "loading" | "ready" | "missing";
type CleanupMode = "disabled" | "cloud";
// "checking"   — initial, calling check_permissions
// "permission" — needs hotkey permission granted
// "main"       — permission OK, show model loading flow
// "mic_denied" — mic access denied mid-session, re-shown to guide user
type PermPhase = "checking" | "permission" | "main" | "mic_denied";
// "idle"    — button ready to press
// "waiting" — button pressed, waiting for OS response
// "granted" — just granted, showing ✓ flash before advancing
type PermCardState = "idle" | "waiting" | "granted";
type PermissionType = "input_monitoring" | "accessibility" | "microphone" | "none";

const CLEANUP_LABELS: Record<CleanupMode, string> = {
  disabled: "Off",
  cloud: "Cloud",
};

export default function Startup() {
  const [whisper, setWhisper] = useState<ModelStatus>("loading");
  const [cleanupMode, setCleanupMode] = useState<CleanupMode | null>(null);
  const [version, setVersion] = useState("");

  const [permPhase, _setPermPhase] = useState<PermPhase>("checking");
  const permPhaseRef = useRef<PermPhase>("checking");
  const setPermPhase = (p: PermPhase) => {
    permPhaseRef.current = p;
    _setPermPhase(p);
  };
  const [permCardState, setPermCardState] = useState<PermCardState>("idle");
  const [permissionType, setPermissionType] = useState<PermissionType>("input_monitoring");

  const allReady = whisper === "ready" && permPhase === "main";
  const inMicDenied = permPhase === "mic_denied";

  useEffect(() => {
    document.body.classList.add("startup-window");
    invoke<string>("get_app_version").then(setVersion);
    invoke<{ cleanup_mode: string }>("get_settings").then((s) => {
      setCleanupMode(s.cleanup_mode as CleanupMode);
    });

    // Check permissions — determines whether to show the wizard or skip to main.
    invoke<{ granted: boolean; permission_type: PermissionType }>("check_permissions")
      .then((perms) => {
        setPermissionType(perms.permission_type);
        setPermPhase(perms.granted ? "main" : "permission");
      })
      .catch(() => setPermPhase("main")); // non-macOS or command error → skip wizard

    // Poll events from the Rust permission loop (emitted every 1s when perm missing).
    const unlistenPerms = listen<{ granted: boolean }>(
      "neuma://permissions",
      (event) => {
        if (event.payload.granted && permPhaseRef.current === "permission") {
          setPermCardState("granted");
          setTimeout(() => {
            setPermCardState("idle");
            setPermPhase("main");
          }, 700);
        }
      },
    );

    // Mic denied — backend re-shows this window and emits this event.
    const unlistenMicDenied = listen("neuma://mic-denied", () => {
      setPermPhase("mic_denied");
    });

    // Whisper model status.
    invoke<{ downloaded: boolean; loaded: boolean }>("get_model_status").then((s) => {
      if (!s.downloaded) setWhisper("missing");
      else if (s.loaded) setWhisper("ready");
    });
    const unlistenWhisper = listen("neuma://model-ready", () => setWhisper("ready"));

    return () => {
      unlistenPerms.then((fn) => fn());
      unlistenWhisper.then((fn) => fn());
      unlistenMicDenied.then((fn) => fn());
    };
  }, []);

  // Auto-hide when model is ready and permissions are done.
  useEffect(() => {
    if (allReady) {
      const t = setTimeout(() => getCurrentWebviewWindow().hide(), 2900);
      return () => clearTimeout(t);
    }
  }, [allReady]);

  // Poll mic permission every second when in mic_denied state.
  // When the user grants access in System Settings, transition back to main.
  useEffect(() => {
    if (!inMicDenied) return;
    const interval = setInterval(() => {
      invoke<string>("check_mic_permission").then((status) => {
        if (status === "authorized") {
          setPermCardState("granted");
          setTimeout(() => {
            setPermCardState("idle");
            setPermPhase("main");
          }, 700);
        }
      });
    }, 1000);
    return () => clearInterval(interval);
  }, [inMicDenied]);

  const handleAllow = async () => {
    setPermCardState("waiting");
    await invoke("request_permissions");
    // Opens System Settings — user must go there and toggle the switch.
    // Rust poll detects the grant and emits neuma://permissions.
    setPermCardState("idle");
  };

  const inPermWizard = permPhase === "permission";

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

        <div className={`startup-status-area${inPermWizard || inMicDenied ? " startup-status-area--perm" : ""}`}>
          <AnimatePresence mode="wait">
            {/* ── Mic denied ── */}
            {inMicDenied && (
              <motion.div
                key="mic_denied"
                initial={{ opacity: 0, x: 28 }}
                animate={{ opacity: 1, x: 0 }}
                exit={{ opacity: 0, x: -28 }}
                transition={{ duration: 0.22, ease: "easeOut" }}
                style={{ width: "100%" }}
              >
                {permCardState === "granted" ? (
                  <div className="startup-status">
                    <Checkmark />
                    <span className="startup-status-text startup-status-text--ready">Access granted</span>
                  </div>
                ) : (
                  <PermissionStep
                    type="microphone"
                    waiting={false}
                    onAllow={() => invoke("open_microphone_settings")}
                  />
                )}
              </motion.div>
            )}

            {/* ── Permission wizard ── */}
            {inPermWizard && (
              <motion.div
                key={permPhase}
                initial={{ opacity: 0, x: 28 }}
                animate={{ opacity: 1, x: 0 }}
                exit={{ opacity: 0, x: -28 }}
                transition={{ duration: 0.22, ease: "easeOut" }}
                style={{ width: "100%" }}
              >
                {permCardState === "granted" ? (
                  <div className="startup-status">
                    <Checkmark />
                    <span className="startup-status-text startup-status-text--ready">Access granted</span>
                  </div>
                ) : (
                  <PermissionStep
                    type={permissionType}
                    waiting={permCardState === "waiting"}
                    onAllow={handleAllow}
                  />
                )}
              </motion.div>
            )}

            {/* ── Model loading / ready / missing ── */}
            {permPhase === "main" && (
              <>
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
              </>
            )}
          </AnimatePresence>
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

// ── Permission step card ───────────────────────────────────────────────────────

const PERM_CONFIG: Record<PermissionType, { icon: string; title: string; desc: string; cta: string }> = {
  input_monitoring: {
    icon: "⌨",
    title: "Input Monitoring",
    desc: "Required to receive keyboard events on macOS 10.15+. Neuma only listens for your hotkey.",
    cta: "Allow Access →",
  },
  accessibility: {
    icon: "🔐",
    title: "Accessibility Access",
    desc: "Required to receive keyboard events. Neuma only listens for your hotkey.",
    cta: "Allow Access →",
  },
  microphone: {
    icon: "🎙",
    title: "Microphone Access",
    desc: "Microphone access was denied. Enable it in System Settings to use Neuma.",
    cta: "Open Mic Settings →",
  },
  none: {
    icon: "🔐",
    title: "Permission Required",
    desc: "Neuma needs permission to detect your hotkey.",
    cta: "Allow Access →",
  },
};

function PermissionStep({
  type,
  waiting,
  onAllow,
}: {
  type: PermissionType;
  waiting: boolean;
  onAllow: () => void;
}) {
  const { icon, title, desc, cta } = PERM_CONFIG[type] ?? PERM_CONFIG.none;
  return (
    <div className="startup-perm-step">
      <div className="startup-perm-icon">{icon}</div>
      <div className="startup-perm-title">{title}</div>
      <div className="startup-perm-desc">{desc}</div>
      <button
        type="button"
        className="startup-btn startup-btn--perm"
        disabled={waiting}
        onClick={onAllow}
      >
        {waiting ? "Waiting…" : cta}
      </button>
    </div>
  );
}

// ── Checkmark SVG ─────────────────────────────────────────────────────────────

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
