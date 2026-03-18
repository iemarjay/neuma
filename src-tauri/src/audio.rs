use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};
use webrtc_vad::{Vad, VadMode};

const TARGET_SAMPLE_RATE: u32 = 16_000;
/// 20ms frame at 16 kHz = 320 samples.
const VAD_FRAME_SAMPLES: usize = 320;
/// 500ms of grace before VAD starts evaluating silence.
const VAD_GRACE_MS: u32 = 500;
/// Consecutive non-speech 20ms frames before auto-stop (75 × 20ms = 1.5s).
const VAD_SILENCE_FRAMES: u32 = 75;

/// Shared state between the stream callback and the recorder handle.
struct Shared {
    /// Accumulated PCM samples at the native device sample rate (mono f32).
    buffer: Vec<f32>,
    /// Latest RMS level computed over the most recent window of samples.
    level: f32,
    /// Whether the stream should still be recording.
    recording: bool,
    /// WebRTC VAD instance (16 kHz, Aggressive mode).
    vad: Vad,
    /// Accumulates native-rate mono samples until a full VAD frame is ready.
    vad_pending: Vec<f32>,
    /// Native-rate samples remaining in the startup grace period.
    grace_remaining: usize,
    /// Consecutive non-speech 20ms frames counted since last speech.
    silence_frames: u32,
    /// 0.0–1.0 fraction of the silence threshold reached. Used to dim waveform.
    silence_progress: f32,
    /// Set to true when silence threshold is reached — signals auto-stop.
    vad_stopped: bool,
}

// Vad contains a raw C pointer and is not Send by default.
// libfvad instances are independent; the Mutex guarantees exclusive access.
unsafe impl Send for Shared {}

/// Push-to-talk / VAD audio recorder.
///
/// Call [`start`] to begin capturing. [`stop`] finishes recording.
/// [`vad_info`] returns `(level, silence_progress, vad_stopped)` for the
/// polling loop — call ~10×/sec.
pub struct AudioRecorder {
    shared: Arc<Mutex<Shared>>,
    native_sample_rate: u32,
    _stream: cpal::Stream,
}

// cpal marks Stream as !Send on macOS via a conservative PhantomData<*mut ()>.
// CoreAudio streams are safe to move across threads.
unsafe impl Send for AudioRecorder {}

impl AudioRecorder {
    /// Open the default input device and begin recording immediately.
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device found"))?;

        let config = device.default_input_config()?;
        let native_sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;

        let mut vad = Vad::new_with_rate(16_000).expect("webrtc-vad: invalid sample rate");
        vad.set_mode(VadMode::Aggressive).expect("webrtc-vad: invalid mode");

        let grace_remaining = (native_sample_rate * VAD_GRACE_MS / 1000) as usize;

        let shared = Arc::new(Mutex::new(Shared {
            buffer: Vec::with_capacity(native_sample_rate as usize * 60),
            level: 0.0,
            recording: true,
            vad,
            vad_pending: Vec::new(),
            grace_remaining,
            silence_frames: 0,
            silence_progress: 0.0,
            vad_stopped: false,
        }));

        let shared_clone = Arc::clone(&shared);
        let err_fn = |e| log::error!("audio stream error: {e}");

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                device.build_input_stream(
                    &config.into(),
                    move |data: &[f32], _: &_| {
                        handle_input_f32(data, channels, native_sample_rate, &shared_clone);
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::I16 => {
                device.build_input_stream(
                    &config.into(),
                    move |data: &[i16], _: &_| {
                        let floats: Vec<f32> = data
                            .iter()
                            .map(|&s| s as f32 / i16::MAX as f32)
                            .collect();
                        handle_input_f32(&floats, channels, native_sample_rate, &shared_clone);
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::U16 => {
                device.build_input_stream(
                    &config.into(),
                    move |data: &[u16], _: &_| {
                        let floats: Vec<f32> = data
                            .iter()
                            .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                            .collect();
                        handle_input_f32(&floats, channels, native_sample_rate, &shared_clone);
                    },
                    err_fn,
                    None,
                )?
            }
            fmt => return Err(anyhow!("unsupported sample format: {fmt:?}")),
        };

        stream.play()?;

        Ok(Self {
            shared,
            native_sample_rate,
            _stream: stream,
        })
    }

    /// Stop recording and return the captured audio resampled to 16 kHz mono f32.
    pub fn stop(self) -> Result<Vec<f32>> {
        {
            let mut s = self.shared.lock().unwrap();
            s.recording = false;
        }
        // _stream is dropped here — cpal stops the stream on drop.
        let buffer = {
            let s = self.shared.lock().unwrap();
            s.buffer.clone()
        };
        let resampled = resample(&buffer, self.native_sample_rate, TARGET_SAMPLE_RATE);
        Ok(resampled)
    }

    /// Returns `(level, silence_progress, vad_stopped)` in one lock acquisition.
    ///
    /// - `level`: RMS amplitude 0.0–1.0 (already dimmed by silence_progress).
    /// - `silence_progress`: 0.0–1.0 fraction of the 1.5s silence window elapsed.
    /// - `vad_stopped`: true when 1.5s of silence has been detected.
    pub fn vad_info(&self) -> (f32, f32, bool) {
        let s = self.shared.lock().unwrap();
        let dimmed = s.level * (1.0 - s.silence_progress * 0.8);
        (dimmed, s.silence_progress, s.vad_stopped)
    }
}

/// Convert multi-channel interleaved samples to mono, push to the shared buffer,
/// update RMS level, and run WebRTC VAD for auto-stop detection.
fn handle_input_f32(
    data: &[f32],
    channels: usize,
    native_sample_rate: u32,
    shared: &Arc<Mutex<Shared>>,
) {
    let mut s = shared.lock().unwrap();
    if !s.recording {
        return;
    }

    // Mix down to mono.
    let mono: Vec<f32> = if channels == 1 {
        data.to_vec()
    } else {
        data.chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    s.buffer.extend_from_slice(&mono);

    // RMS over last 1024 samples.
    let window_size = 1024_usize;
    let start = s.buffer.len().saturating_sub(window_size);
    let window = &s.buffer[start..];
    let rms = (window.iter().map(|&x| x * x).sum::<f32>() / window.len() as f32).sqrt();
    s.level = (rms * 4.0).min(1.0);

    // ── VAD ──────────────────────────────────────────────────────────────────
    if s.vad_stopped {
        return;
    }

    if s.grace_remaining > 0 {
        // Still in the startup grace period — consume samples without evaluating.
        let consumed = mono.len().min(s.grace_remaining);
        s.grace_remaining -= consumed;
        return;
    }

    s.vad_pending.extend_from_slice(&mono);

    // Number of native-rate samples that correspond to VAD_FRAME_SAMPLES at 16 kHz.
    // E.g. at 44 100 Hz: 320 × 44100 / 16000 = 882 samples.
    let frame_native = (VAD_FRAME_SAMPLES as u64 * native_sample_rate as u64
        / TARGET_SAMPLE_RATE as u64) as usize;

    while s.vad_pending.len() >= frame_native {
        let frame: Vec<f32> = s.vad_pending.drain(..frame_native).collect();
        let mut frame_16k = resample(&frame, native_sample_rate, TARGET_SAMPLE_RATE);
        // Ensure exactly VAD_FRAME_SAMPLES — resample may produce N±1 due to rounding.
        frame_16k.truncate(VAD_FRAME_SAMPLES);
        if frame_16k.len() < VAD_FRAME_SAMPLES {
            continue; // Incomplete frame; skip.
        }

        let frame_i16: Vec<i16> = frame_16k
            .iter()
            .map(|&x| (x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();

        match s.vad.is_voice_segment(&frame_i16) {
            Ok(true) => {
                s.silence_frames = 0;
                s.silence_progress = 0.0;
            }
            Ok(false) | Err(_) => {
                s.silence_frames += 1;
                s.silence_progress = (s.silence_frames as f32 / VAD_SILENCE_FRAMES as f32).min(1.0);
                if s.silence_frames >= VAD_SILENCE_FRAMES {
                    s.vad_stopped = true;
                    break;
                }
            }
        }
    }
}

/// Linear interpolation resample from `src_rate` Hz to `dst_rate` Hz (mono f32).
fn resample(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if src_rate == dst_rate {
        return input.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let output_len = (input.len() as f64 / ratio).ceil() as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_pos = i as f64 * ratio;
        let src_idx = src_pos as usize;
        let frac = (src_pos - src_idx as f64) as f32;

        let a = input.get(src_idx).copied().unwrap_or(0.0);
        let b = input.get(src_idx + 1).copied().unwrap_or(0.0);
        output.push(a + frac * (b - a));
    }

    output
}
