# Neuma

Offline-first voice dictation desktop app — a Wispr Flow replacement. Press a hotkey to start recording, press again to transcribe (toggle mode, default). Or hold and release for push-to-talk. Your words appear wherever your cursor is. No cloud required. Runs entirely on-device via Whisper Turbo (GGUF). Optional text cleanup via Cloudflare Workers AI when online.

## Stack

| Layer | Tool | Notes |
|---|---|---|
| Desktop framework | Tauri 2 | Rust backend + React WebView frontend |
| Async runtime | Tokio | Built into Tauri |
| Global hotkeys | `rdev` | Single key, tap-vs-hold detection: tap (<400ms) = toggle mode, hold (≥400ms) = push-to-talk mode |
| Audio capture | `cpal` | Default mic, 16kHz mono f32 PCM |
| Transcription | `whisper-rs` | Whisper Turbo GGUF — fully offline, loaded at startup |
| Clipboard | `arboard` | Cross-platform read/write |
| Paste simulation | `enigo 0.2` | Cmd+V (macOS) / Ctrl+V (Win/Linux) |
| HTTP client | `reqwest` | CF Worker cleanup calls |
| Settings persistence | `tauri-plugin-store` | JSON store — hotkey, CF URL, cleanup toggle |
| Text cleanup | CF Worker → Workers AI | Optional — `@cf/meta/llama-3.2-1b-instruct` |
| UI animation | `framer-motion` | State transitions, waveform, checkmark |
| Frontend build | Vite + TypeScript | Standard Tauri v2 frontend setup |

## UX State Machine

```
Idle (hidden)
  │
  │ hotkey pressed
  ▼
Loading (hidden) ── if model not yet loaded
  │
  │ model ready
  ▼
Listening ── emit audioLevel events (~10/sec) ──► waveform animation
  │            tap mode: cancel button visible; PTT mode: confirmed at 400ms, cancel hidden
  │
  │ tap mode: second press    |    PTT mode: key release
  ▼
Transcribing ── whisper-rs on buffered PCM ──► bouncing dots
  │
  ├─ cleanup disabled / offline ──────────────────────────────┐
  │                                                            │
  │ cleanup enabled + online                                   │
  ▼                                                            │
Cleaning ── CF Worker → Workers AI cleanup ──► shimmer       │
  │                                                            │
  └────────────────────────────────────────────────────────────┘
  ▼
Done ── arboard sets clipboard → enigo pastes → animated checkmark
  │
  │ 2s
  ▼
Idle (hidden, clipboard restored)
```

Error at any stage → red overlay → auto-hide after 2s → Idle.

## Overlay UI

The entire frontend is one component: a pill-shaped overlay that lives at the bottom center of the primary monitor. It is always-on-top, transparent, decoration-free, and only visible during active dictation.

| State | Visual |
|---|---|
| Listening | Animated waveform (5 bars, RMS-driven), mic icon, "Listening" |
| Transcribing | 3 bouncing dots, "Transcribing" |
| Cleaning | Shimmer/sparkle, "Polishing" |
| Done | Animated checkmark SVG (draws itself), "Done", fades out after 800ms |
| Error | Red tint, X icon, error message, auto-hide after 2s |

Style: glassmorphism — `backdrop-filter: blur(20px)`, `#0a0a0a` at 80% opacity, 10% white border, white text, `border-radius: 999px`, ~360×56px. framer-motion AnimatePresence drives state transitions.

## File Layout

```
src/                          # React frontend
  main.tsx                    # React entry point
  App.tsx                     # Overlay component (entire UI)
  index.css                   # Reset, CSS vars, keyframe animations

src-tauri/
  src/
    main.rs                   # Binary entry — calls lib::run()
    lib.rs                    # Tauri setup, hotkey wiring, pipeline orchestration
    hotkey_listener.rs        # rdev listener — single key, tap-vs-hold mode detection
    audio.rs                  # cpal recorder — capture, resample to 16kHz, RMS level
    transcribe.rs             # whisper-rs wrapper — load model, transcribe PCM
    cleanup.rs                # reqwest → CF Worker text cleanup + connectivity check
    typer.rs                  # arboard + enigo — clipboard set + paste simulation
    settings.rs               # Settings struct (Serialize/Deserialize + Default)
  Cargo.toml
  build.rs
  tauri.conf.json
  capabilities/
    default.json              # Tauri v2 capability grants

worker/
  index.ts                    # CF Worker — POST /cleanup → Workers AI

models/                       # GGUF model files (not committed — gitignored)
  .gitkeep

wrangler.toml
package.json
vite.config.ts
tsconfig.json
index.html
.gitignore
```

## Settings Schema

Persisted via `tauri-plugin-store` in `settings.json` in the app data directory.

```json
{
  "hotkey": "alt",
  "cf_worker_url": "",
  "cleanup_enabled": false,
  "model_path": "models/ggml-whisper-turbo.bin",
  "launch_at_login": false
}
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `hotkey` | string | `"alt"` | Single key — tap (<400ms) toggles recording on/off; hold (≥400ms) switches to push-to-talk, releases to stop. Handled by `rdev` (CGEventTap on macOS). Valid: `"fn"`, `"alt"`, `"right_alt"`, `"ctrl"`, `"right_ctrl"`. |
| `cf_worker_url` | string | `""` | Full URL to deployed CF Worker, e.g. `https://neuma-cleanup.workers.dev` |
| `cleanup_enabled` | bool | `false` | If false, skip cleanup stage entirely |
| `model_path` | string | `"models/ggml-whisper-turbo.bin"` | Relative to app data dir, or absolute path |
| `launch_at_login` | bool | `false` | Auto-start Neuma at login via LaunchAgent (macOS). Also togglable from the menu bar. |

## Model Download

Neuma uses Whisper Turbo in GGUF format. The model is not bundled — download it once:

```bash
# Create models dir in project root (for dev)
mkdir -p models

# Download Whisper Turbo GGUF (~800MB)
curl -L -o models/ggml-whisper-turbo.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin

# Or smaller for faster inference / less RAM:
# ggml-base.en.bin (~142MB) — English only, fastest
# ggml-small.en.bin (~466MB) — better accuracy
# ggml-medium.en.bin (~1.5GB) — near-turbo quality
```

The app looks for the model at the path in settings (`model_path`). During dev, place the file at `models/ggml-whisper-turbo.bin` in the project root — `lib.rs` resolves it relative to the app data dir or falls back to the working directory.

## CF Worker Setup

The cleanup worker is optional. Only deploy if you want AI-powered filler word removal.

```bash
# Install Wrangler
npm install -g wrangler

# Login
wrangler login

# Deploy
npx wrangler deploy

# Worker URL will be printed — add it to Neuma settings
```

The worker exposes one endpoint:

```
POST /cleanup
Content-Type: application/json

{ "text": "um so like I wanted to uh talk about..." }

→ { "result": "I wanted to talk about..." }
```

It uses `@cf/meta/llama-3.2-1b-instruct` — fast, free on Workers AI free tier.

## Dev Commands

```bash
# Install JS deps
npm install

# Desktop app (hot reload)
cargo tauri dev

# Build distributable
cargo tauri build

# CF Worker local dev
npx wrangler dev

# CF Worker deploy
npx wrangler deploy

# Check Rust only
cd src-tauri && cargo check
cd src-tauri && cargo clippy
```

## Key Decisions

- **Fully offline transcription.** Whisper Turbo runs on-device via whisper-rs (llama.cpp bindings). No audio ever leaves the machine unless cleanup is explicitly enabled.
- **Single key, tap-vs-hold.** One configurable key drives both modes. Tap (release <400ms) = toggle: starts recording on first press, stops on second. Hold (≥400ms) = push-to-talk: stops recording on release. `rdev` (CGEventTap on macOS) handles the listener — `tauri-plugin-global-shortcut`/muda is not used because its hardcoded key list excludes standalone modifier keys like `fn`, `alt`, `ctrl`. Requires Accessibility permission on macOS.
- **Clipboard injection.** Injects text by writing to clipboard and simulating Cmd+V / Ctrl+V. Works in every app including Electron apps, terminals, browsers. arboard handles cross-platform clipboard; enigo handles keystroke simulation.
- **Single overlay window + menu bar tray.** The entire dictation UI is one pill-shaped overlay that appears only during active dictation. A menu bar tray icon provides a persistent presence for Quit and Launch at Login without a Dock icon (`ActivationPolicy::Accessory`).
- **whisper-rs over HTTP API.** Running Whisper in-process (not as a sidecar subprocess) means faster startup, no port management, and no extra binary to ship.
- **CF Worker for cleanup only.** No audio hits the network. Only the final transcript text (after local Whisper runs) can optionally be sent for cleanup, and only when the user explicitly enables it.
- **tauri-plugin-store for settings.** Simple JSON persistence — no SQLite needed for settings-level data. Schema is flat and small.

## Audio Pipeline

```
cpal default input device
  → native sample rate (e.g. 44100/48000 Hz), f32 mono
  → linear interpolation resample to 16000 Hz
  → buffered in Vec<f32>
  → on stop() → returned to pipeline
  → whisper-rs WhisperContext::transcribe()
```

RMS level is computed over the last 1024 samples ~10 times per second and emitted as `neuma://audio-level` events so the frontend waveform animation responds in real time.

## Tauri Events

| Event | Direction | Payload |
|---|---|---|
| `neuma://state` | Rust → React | `{ state: "idle" \| "loading" \| "listening" \| "transcribing" \| "cleaning" \| "done" \| "error", mode?: "toggle" \| "ptt", message?: string }` — `mode` present on `listening`; `message` present on `error` |
| `neuma://audio-level` | Rust → React | `{ level: number }` (0.0–1.0 RMS) |

## MVP Checklist

- [ ] Whisper model loads at startup, warns if missing
- [ ] Single hotkey listener via rdev (default `alt`): tap to toggle, hold for push-to-talk
- [ ] Tap-vs-hold threshold (400ms) switches mode mid-press; cancel button visible in toggle mode only
- [ ] cpal records mic audio, resamples to 16kHz mono f32
- [ ] RMS level events drive waveform animation in frontend
- [ ] whisper-rs transcribes buffered PCM
- [ ] arboard + enigo injects text via clipboard paste
- [ ] Original clipboard content restored after paste
- [ ] CF Worker cleanup (optional, guarded by connectivity check)
- [ ] Overlay appears at bottom-center of the monitor containing the cursor (falls back to primary)
- [ ] framer-motion state transitions: listening → transcribing → cleaning → done → idle
- [ ] Settings persisted via tauri-plugin-store
- [ ] get_settings / save_settings / get_app_version / cancel_recording Tauri commands
- [ ] Menu bar tray icon with Launch at Login toggle and Quit
- [ ] Launch at login via tauri-plugin-autostart (LaunchAgent on macOS)
- [ ] Error states displayed and auto-dismissed

## Out of Scope (MVP)

- Settings UI (edit hotkey/CF URL in-app) — edit settings.json directly for now
- Custom vocabulary / Whisper prompt injection
- Streaming transcription (partial results while speaking)
- VAD (voice activity detection) for auto-stop
- Noise suppression / pre-processing
- App icon

## Post-MVP Plan

- **Accessibility API text injection (macOS).** Use `AXUIElement` via `objc2` bindings to inject text directly into focused native text fields, bypassing the clipboard entirely. This mirrors how Wispr Flow works. Trigger only when clipboard injection fails or for native macOS apps (TextEdit, Mail, Notes, Safari address bar). Clipboard path remains the fallback for Electron apps (VS Code, Slack, etc.) which have shallow AX trees. Requires prompting user for Accessibility permission in System Settings and `com.apple.security.temporary-exception.accessibility` entitlement in the signed build.
- **Personal dictionary.** Store a user-defined list of terms (`dictionary: Vec<String>` in settings) for names, jargon, and unusual spellings. Applied two ways: (1) passed as Whisper `initial_prompt` to bias transcription offline; (2) passed to the CF Worker cleanup prompt as a glossary. Passive auto-learning (detecting post-injection edits via AXUIElement diff and adding changed words automatically) requires AX integration — manual additions via settings are the MVP path.
- **Context-aware spelling.** Before injecting, read the focused field's existing text via `AXUIElement` and send it alongside the transcript to the CF Worker. The LLM uses names and terms already present in the document/thread to correct Whisper's phonetic guesses (e.g., "Daveed" already in the thread corrects "David" in the transcript). Requires AX integration.
- Settings UI — in-app editor for hotkey, CF Worker URL, cleanup toggle, model path, dictionary management
- Streaming transcription (partial results while speaking)
- VAD (voice activity detection) for auto-stop
- Noise suppression / pre-processing
- App icon
