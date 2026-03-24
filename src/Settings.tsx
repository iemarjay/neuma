import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface Settings {
  hotkey: string;
  cleanup_api_key: string;
  cleanup_mode: string; // "disabled" | "cloud"
  launch_at_login: boolean;
}

interface ModelStatus {
  downloaded: boolean;
}

interface DownloadProgress {
  downloaded: number;
  total: number;
  speedBps: number;
  etaSecs: number;
}

type DownloadState =
  | { kind: "idle" }
  | { kind: "downloading"; progress: DownloadProgress }
  | { kind: "error"; message: string };

type TestState =
  | { kind: "idle" }
  | { kind: "testing" }
  | { kind: "ok" }
  | { kind: "error"; message: string };

export default function Settings() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [modelDownloaded, setModelDownloaded] = useState(false);
  const [downloadState, setDownloadState] = useState<DownloadState>({ kind: "idle" });
  const [testState, setTestState] = useState<TestState>({ kind: "idle" });
  const [appVersion, setAppVersion] = useState("");

  useEffect(() => {
    document.body.classList.add("settings-window");
    invoke<Settings>("get_settings").then(setSettings);
    invoke<ModelStatus>("get_model_status").then((s) => setModelDownloaded(s.downloaded));
    invoke<string>("get_app_version").then(setAppVersion);

    const unlistenProgress = listen<DownloadProgress>("neuma://download-progress", (e) => {
      setDownloadState({ kind: "downloading", progress: e.payload });
    });
    const unlistenComplete = listen<void>("neuma://download-complete", () => {
      setModelDownloaded(true);
      setDownloadState({ kind: "idle" });
    });
    const unlistenError = listen<{ message: string }>("neuma://download-error", (e) => {
      setDownloadState({ kind: "error", message: e.payload.message });
    });

    return () => {
      document.body.classList.remove("settings-window");
      unlistenProgress.then((f) => f());
      unlistenComplete.then((f) => f());
      unlistenError.then((f) => f());
    };
  }, []);

  const saveSettings = (updated: Settings) => {
    setSettings(updated);
    invoke("save_settings", { newSettings: updated }).catch(console.error);
  };

  const handleModeChange = (mode: string) => {
    if (!settings) return;
    saveSettings({ ...settings, cleanup_mode: mode });
  };

  const handleApiKeyBlur = () => {
    if (settings) saveSettings(settings);
  };

  const handleDownload = async () => {
    setDownloadState({
      kind: "downloading",
      progress: { downloaded: 0, total: 825_000_000, speedBps: 0, etaSecs: 0 },
    });
    await invoke("download_model").catch((e) => {
      setDownloadState({ kind: "error", message: String(e) });
    });
  };

  const handleCancelDownload = async () => {
    await invoke("cancel_model_download");
    setDownloadState({ kind: "idle" });
  };

  const handleTestConnection = async () => {
    if (!settings) return;
    setTestState({ kind: "testing" });
    try {
      await invoke("test_cleanup_connection", { apiKey: settings.cleanup_api_key });
      setTestState({ kind: "ok" });
      setTimeout(() => setTestState({ kind: "idle" }), 3000);
    } catch (e) {
      setTestState({ kind: "error", message: String(e) });
    }
  };

  if (!settings) return null;

  return (
    <div className="settings">
      <section className="settings-section">
        <h2 className="section-title">Transcription Model</h2>
        <ModelSection
          downloaded={modelDownloaded}
          downloadState={downloadState}
          onDownload={handleDownload}
          onCancel={handleCancelDownload}
        />
      </section>

      <div className="divider" />

      <section className="settings-section">
        <h2 className="section-title">Text Cleanup</h2>

        <div className="mode-selector">
          {(["disabled", "cloud"] as const).map((mode) => (
            <button
              key={mode}
              type="button"
              className={`mode-btn ${settings.cleanup_mode === mode ? "mode-btn--active" : ""}`}
              onClick={() => handleModeChange(mode)}
            >
              {mode === "disabled" ? "Off" : "Cloud"}
            </button>
          ))}
        </div>

        {settings.cleanup_mode === "cloud" && (
          <div className="cleanup-sub">
            <div className="field-row">
              <div className="api-key-group">
                <input
                  type="password"
                  className="api-key-input"
                  value={settings.cleanup_api_key}
                  onChange={(e) => setSettings({ ...settings, cleanup_api_key: e.target.value })}
                  onBlur={handleApiKeyBlur}
                  placeholder="API key"
                  spellCheck={false}
                />
                <button
                  type="button"
                  className={`test-btn test-btn--${testState.kind}`}
                  onClick={handleTestConnection}
                  disabled={!settings.cleanup_api_key || testState.kind === "testing"}
                >
                  {testState.kind === "testing"
                    ? "…"
                    : testState.kind === "ok"
                      ? "✓"
                      : testState.kind === "error"
                        ? "✗"
                        : "Test"}
                </button>
              </div>
              {testState.kind === "error" && (
                <p className="field-error">{testState.message}</p>
              )}
            </div>
          </div>
        )}
      </section>

      <div className="divider" />

      <section className="settings-section">
        <h2 className="section-title">Hotkey</h2>
        <div className="mode-selector">
          {(["fn", "alt", "right_alt", "ctrl", "right_ctrl"] as const).map((key) => (
            <button
              key={key}
              type="button"
              className={`mode-btn ${settings.hotkey === key ? "mode-btn--active" : ""}`}
              onClick={() => saveSettings({ ...settings, hotkey: key })}
            >
              {key === "fn" ? "Fn" : key === "alt" ? "Alt" : key === "right_alt" ? "R.Alt" : key === "ctrl" ? "Ctrl" : "R.Ctrl"}
            </button>
          ))}
        </div>
      </section>

      <div className="divider" />

      <section className="settings-section">
        <h2 className="section-title">General</h2>
        <div className="toggle-row">
          <span className="toggle-label">Launch at Login</span>
          <button
            type="button"
            className={`toggle ${settings.launch_at_login ? "toggle--on" : "toggle--off"}`}
            onClick={() => saveSettings({ ...settings, launch_at_login: !settings.launch_at_login })}
            title="Launch at Login"
          />
        </div>
      </section>

      <footer className="settings-footer">
        <span className="version-text">Neuma {appVersion ? `v${appVersion}` : ""}</span>
      </footer>
    </div>
  );
}

function ModelSection({
  name = "Whisper Turbo",
  sizeMb = 800,
  downloaded,
  downloadState,
  onDownload,
  onCancel,
}: {
  name?: string;
  sizeMb?: number;
  downloaded: boolean;
  downloadState: DownloadState;
  onDownload: () => void;
  onCancel: () => void;
}) {
  if (downloadState.kind === "downloading") {
    const { downloaded: dl, total, speedBps, etaSecs } = downloadState.progress;
    const pct = total > 0 ? Math.round((dl / total) * 100) : 0;
    const speed = formatBytes(speedBps) + "/s";
    const eta = etaSecs > 0 ? formatEta(etaSecs) + " remaining" : "calculating…";

    return (
      <div className="model-downloading">
        <div className="model-download-meta">
          <span className="model-name">{name}</span>
          <span className="model-download-stats">
            {pct}% · {speed} · {eta}
          </span>
        </div>
        <div className="progress-bar">
          <div className="progress-fill" style={{ "--pct": `${pct}%` } as React.CSSProperties} />
        </div>
        <button type="button" className="btn btn--ghost btn--sm" onClick={onCancel}>
          Cancel
        </button>
      </div>
    );
  }

  if (downloadState.kind === "error") {
    return (
      <div className="model-row">
        <span className="field-error">Download failed: {downloadState.message}</span>
        <button type="button" className="btn btn--primary" onClick={onDownload}>
          Retry
        </button>
      </div>
    );
  }

  if (downloaded) {
    return (
      <div className="model-row">
        <span className="model-status">
          <span className="model-check">✓</span> {name}
        </span>
        <button type="button" className="btn btn--ghost btn--sm" onClick={onDownload}>
          Re-download
        </button>
      </div>
    );
  }

  return (
    <div className="model-row">
      <span className="model-hint">~{sizeMb} MB · Required</span>
      <button type="button" className="btn btn--primary" onClick={onDownload}>
        Download
      </button>
    </div>
  );
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function formatEta(secs: number): string {
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  return `${m}m ${s}s`;
}
