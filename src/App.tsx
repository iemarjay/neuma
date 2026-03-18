import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { AnimatePresence, motion } from "framer-motion";

// ─── Types ────────────────────────────────────────────────────────────────────

type ListenMode = "toggle" | "ptt";

// Mirrors the NeumaState enum in lib.rs — serde tag = "state".
// TypeScript narrows fields per variant, making impossible accesses a compile error.
type NeumaState =
  | { state: "idle" }
  | { state: "loading" }
  | { state: "listening"; mode: ListenMode }
  | { state: "transcribing" }
  | { state: "cleaning" }
  | { state: "done" }
  | { state: "error"; message: string };

// The discriminant string — used for rendering and style selection.
type AppStateName = NeumaState["state"];

interface AudioLevelEvent {
  level: number; // 0.0 – 1.0 RMS
}

// ─── Pill container variants ─────────────────────────────────────────────────

const pillVariants = {
  hidden: {
    opacity: 0,
    y: 16,
    scale: 0.94,
    filter: "blur(4px)",
  },
  visible: {
    opacity: 1,
    y: 0,
    scale: 1,
    filter: "blur(0px)",
    transition: { type: "spring", stiffness: 420, damping: 28 },
  },
  shake: {
    opacity: 1,
    y: 0,
    scale: 1,
    filter: "blur(0px)",
    x: [0, -8, 8, -6, 6, -3, 3, 0],
    transition: { duration: 0.45, ease: "easeInOut" },
  },
  exit: {
    opacity: 0,
    y: 12,
    scale: 0.96,
    filter: "blur(6px)",
    transition: { duration: 0.28, ease: "easeIn" },
  },
};

// ─── Content variants (inner content fades when state changes) ────────────────

const contentVariants = {
  hidden: { opacity: 0, y: 6 },
  visible: { opacity: 1, y: 0, transition: { duration: 0.22, ease: "easeOut" } },
  exit: { opacity: 0, y: -4, transition: { duration: 0.16, ease: "easeIn" } },
};

// ─── Mic icon ─────────────────────────────────────────────────────────────────

function MicIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" style={{ flexShrink: 0 }}>
      <path d="M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z" />
      <path d="M19 10v2a7 7 0 0 1-14 0v-2" />
      <line x1="12" y1="19" x2="12" y2="23" />
      <line x1="8" y1="23" x2="16" y2="23" />
    </svg>
  );
}


// ─── Waveform ─────────────────────────────────────────────────────────────────

function Waveform({ level }: { level: number }) {
  // Drive bar heights from RMS level with some variance per bar
  const multipliers = [0.55, 0.85, 1.0, 0.85, 0.55];
  const noiseOffsets = [0.15, 0.05, 0.0, 0.08, 0.18];
  const minH = 3;
  const maxH = 22;

  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: "3px",
        height: "22px",
      }}
    >
      {multipliers.map((mult, i) => {
        const driven = level > 0.01;
        const h = driven
          ? Math.max(minH, Math.min(maxH, (level + noiseOffsets[i]) * maxH * mult))
          : undefined;

        return (
          <motion.div
            key={i}
            className={driven ? undefined : "waveform-bar"}
            animate={driven ? { height: h } : undefined}
            transition={{ duration: 0.08, ease: "easeOut" }}
            style={{
              width: "3px",
              height: driven ? h : undefined,
              borderRadius: "99px",
              background: "rgba(255,255,255,0.90)",
              flexShrink: 0,
            }}
          />
        );
      })}
    </div>
  );
}

// ─── Animated X mark ──────────────────────────────────────────────────────────

function AnimatedXMark() {
  return (
    <svg width="20" height="20" viewBox="0 0 24 24" fill="none" style={{ flexShrink: 0 }}>
      <motion.circle
        cx="12" cy="12" r="10"
        stroke="rgba(255,100,100,0.7)"
        strokeWidth="1.5"
        fill="none"
        initial={{ pathLength: 0, opacity: 0 }}
        animate={{ pathLength: 1, opacity: 1 }}
        transition={{ duration: 0.35, ease: "easeOut" }}
      />
      <motion.line
        x1="8" y1="8" x2="16" y2="16"
        stroke="rgba(255,120,120,0.95)"
        strokeWidth="2.2"
        strokeLinecap="round"
        initial={{ pathLength: 0, opacity: 0 }}
        animate={{ pathLength: 1, opacity: 1 }}
        transition={{ duration: 0.22, ease: "easeOut", delay: 0.2 }}
      />
      <motion.line
        x1="16" y1="8" x2="8" y2="16"
        stroke="rgba(255,120,120,0.95)"
        strokeWidth="2.2"
        strokeLinecap="round"
        initial={{ pathLength: 0, opacity: 0 }}
        animate={{ pathLength: 1, opacity: 1 }}
        transition={{ duration: 0.22, ease: "easeOut", delay: 0.34 }}
      />
    </svg>
  );
}


// ─── State content components ─────────────────────────────────────────────────

function ListeningContent({ level }: { level: number }) {
  return (
    <motion.div
      key="listening"
      variants={contentVariants}
      initial="hidden"
      animate="visible"
      exit="exit"
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        gap: "8px",
        padding: "0 16px",
        width: "100%",
      }}
    >
      <motion.div
        animate={{ opacity: [0.7, 1, 0.7] }}
        transition={{ duration: 1.6, repeat: Infinity, ease: "easeInOut" }}
        className="mic-icon-wrap"
      >
        <MicIcon />
      </motion.div>
      <Waveform level={level} />
    </motion.div>
  );
}

function TranscribingContent() {
  return (
    <motion.div
      key="transcribing"
      variants={contentVariants}
      initial="hidden"
      animate="visible"
      exit="exit"
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        gap: "14px",
        padding: "0 20px",
        width: "100%",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: "5px", height: "24px" }}>
        {[0, 1, 2].map((i) => (
          <div
            key={i}
            className="transcribe-dot"
            style={{
              width: "6px",
              height: "6px",
              borderRadius: "50%",
              background: "rgba(255,255,255,0.85)",
            }}
          />
        ))}
      </div>
    </motion.div>
  );
}

function CleaningContent() {
  return (
    <motion.div
      key="cleaning"
      variants={contentVariants}
      initial="hidden"
      animate="visible"
      exit="exit"
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        gap: "14px",
        padding: "0 20px",
        width: "100%",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: "5px", height: "24px" }}>
        {[0, 1, 2].map((i) => (
          <div
            key={i}
            className="transcribe-dot"
            style={{
              width: "6px",
              height: "6px",
              borderRadius: "50%",
              background: "rgba(200,185,255,0.85)",
            }}
          />
        ))}
      </div>
    </motion.div>
  );
}


function ErrorContent() {
  return (
    <motion.div
      key="error"
      variants={contentVariants}
      initial="hidden"
      animate="visible"
      exit="exit"
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        padding: "0 14px",
        width: "100%",
      }}
    >
      <AnimatedXMark />
    </motion.div>
  );
}

// ─── Pill styles per state ────────────────────────────────────────────────────

function getPillStyle(state: AppStateName): React.CSSProperties {
  const base: React.CSSProperties = {
    width: "fit-content",
    minWidth: "120px",
    maxWidth: "600px",
    height: "var(--pill-height)",
    borderRadius: "var(--pill-radius)",
    backdropFilter: `blur(var(--blur))`,
    WebkitBackdropFilter: `blur(var(--blur))`,
    display: "flex",
    alignItems: "center",
    overflow: "hidden",
    position: "relative",
    willChange: "transform, opacity",
    cursor: "default",
    userSelect: "none",
  };

  if (state === "error") {
    return { ...base, background: "var(--error-bg)", border: "1px solid var(--error-border)" };
  }

  if (state === "cleaning") {
    return { ...base, background: "rgba(12, 10, 20, 0.82)", border: "1px solid rgba(180,160,255,0.15)" };
  }

  return { ...base, background: "var(--bg)", border: "1px solid var(--border)" };
}

// ─── App ──────────────────────────────────────────────────────────────────────

export default function App() {
  const [appState, setAppState] = useState<AppStateName>("idle");
  const [listenMode, setListenMode] = useState<ListenMode | null>(null);
  const [audioLevel, setAudioLevel] = useState(0);
  const hideTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(() => {
    // Listen for state transitions from Rust backend.
    // payload is NeumaState — TypeScript narrows mode/message per variant.
    const unlistenState = listen<NeumaState>("neuma://state", ({ payload }) => {
      if (hideTimerRef.current) {
        clearTimeout(hideTimerRef.current);
        hideTimerRef.current = null;
      }

      setAppState(payload.state);

      if (payload.state === "listening") {
        setListenMode(payload.mode);
      } else {
        setListenMode(null);
      }


      if (payload.state === "error") {
        // payload.message is only accessible here — TS narrows to { state: "error"; message: string }
        console.error("[neuma] error:", payload.message);
        hideTimerRef.current = setTimeout(() => {
          setAppState("idle");
          setAudioLevel(0);
        }, 2200);
      }

      if (payload.state === "done") {
        hideTimerRef.current = setTimeout(() => {
          setAppState("idle");
          setAudioLevel(0);
        }, 2000);
      }

      if (payload.state === "idle" || payload.state === "loading") {
        setAudioLevel(0);
      }
    });

    // Listen for audio level events (~10/sec from Rust while recording)
    const unlistenLevel = listen<AudioLevelEvent>("neuma://audio-level", ({ payload }) => {
      setAudioLevel(payload.level);
    });

    return () => {
      unlistenState.then((fn) => fn());
      unlistenLevel.then((fn) => fn());
      if (hideTimerRef.current) clearTimeout(hideTimerRef.current);
    };
  }, []);

  const isVisible = appState !== "idle" && appState !== "loading" && appState !== "done";

  return (
    <div
      style={{
        width: "100vw",
        height: "100vh",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        background: "transparent",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
        {/* Done button — left of pill, toggle mode only */}
        <AnimatePresence>
          {appState === "listening" && listenMode === "toggle" && (
            <motion.button
              key="done-outer"
              initial={{ opacity: 0, scale: 0.7 }}
              animate={{ opacity: 1, scale: 1 }}
              exit={{ opacity: 0, scale: 0.7 }}
              transition={{ duration: 0.18, ease: "easeOut" }}
              whileHover={{ scale: 1.1, opacity: 1 }}
              whileTap={{ scale: 0.88 }}
              onClick={() => invoke("stop_recording_and_transcribe")}
              className="done-btn"
              title="Done"
            >
              <svg width="11" height="11" viewBox="0 0 11 11" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                <polyline points="1.5,6 4,8.5 9.5,2" />
              </svg>
            </motion.button>
          )}
        </AnimatePresence>

        <AnimatePresence>
          {isVisible && (
            <motion.div
              key="pill"
              variants={pillVariants}
              initial="hidden"
              animate={appState === "error" ? "shake" : "visible"}
              exit="exit"
              style={getPillStyle(appState)}
              layout
              layoutId="pill"
            >
              {/* Subtle inner highlight at top edge */}
              <div
                style={{
                  position: "absolute",
                  top: 0,
                  left: "10%",
                  right: "10%",
                  height: "1px",
                  background: "linear-gradient(90deg, transparent, rgba(255,255,255,0.12), transparent)",
                  borderRadius: "999px",
                  pointerEvents: "none",
                }}
              />

              <AnimatePresence mode="wait">
                {appState === "listening" && (
                  <ListeningContent key="listening" level={audioLevel} />
                )}
                {appState === "transcribing" && (
                  <TranscribingContent key="transcribing" />
                )}
                {appState === "cleaning" && (
                  <CleaningContent key="cleaning" />
                )}
                {appState === "error" && (
                  <ErrorContent key="error" />
                )}
              </AnimatePresence>
            </motion.div>
          )}
        </AnimatePresence>

        {/* Cancel button — outside the pill, visible in both toggle and PTT modes */}
        <AnimatePresence>
          {appState === "listening" && (
            <motion.button
              key="cancel-outer"
              initial={{ opacity: 0, scale: 0.7 }}
              animate={{ opacity: 1, scale: 1 }}
              exit={{ opacity: 0, scale: 0.7 }}
              transition={{ duration: 0.18, ease: "easeOut" }}
              whileHover={{ scale: 1.1, opacity: 1 }}
              whileTap={{ scale: 0.88 }}
              onClick={() => invoke("cancel_recording")}
              className="cancel-btn"
              title="Cancel"
            >
              <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round">
                <line x1="1" y1="1" x2="9" y2="9" />
                <line x1="9" y1="1" x2="1" y2="9" />
              </svg>
            </motion.button>
          )}
        </AnimatePresence>
      </div>
    </div>
  );
}
