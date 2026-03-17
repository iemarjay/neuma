use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Shared state between the stream callback and the recorder handle.
struct Shared {
    /// Accumulated PCM samples at the native device sample rate (mono f32).
    buffer: Vec<f32>,
    /// Latest RMS level computed over the most recent window of samples.
    level: f32,
    /// Whether the stream should still be recording.
    recording: bool,
}

/// Push-to-talk audio recorder.
///
/// Call [`start`] to begin capturing, [`stop`] to finish. [`level`] returns
/// the current RMS amplitude (0.0–1.0) for waveform animation.
pub struct AudioRecorder {
    shared: Arc<Mutex<Shared>>,
    native_sample_rate: u32,
    _stream: cpal::Stream,
}

// cpal marks Stream as !Send on macOS via a conservative PhantomData<*mut ()>.
// CoreAudio streams are safe to move across threads; the stream callbacks run
// on CoreAudio's own private threads regardless. This is a known cpal limitation.
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

        let shared = Arc::new(Mutex::new(Shared {
            buffer: Vec::with_capacity(native_sample_rate as usize * 60),
            level: 0.0,
            recording: true,
        }));

        let shared_clone = Arc::clone(&shared);

        let err_fn = |e| log::error!("audio stream error: {e}");

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                device.build_input_stream(
                    &config.into(),
                    move |data: &[f32], _: &_| {
                        handle_input_f32(data, channels, &shared_clone);
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
                        handle_input_f32(&floats, channels, &shared_clone);
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
                        handle_input_f32(&floats, channels, &shared_clone);
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

    /// Current RMS level (0.0–1.0). Call ~10×/sec for waveform animation.
    pub fn level(&self) -> f32 {
        self.shared.lock().unwrap().level
    }
}

/// Convert multi-channel interleaved samples to mono and push to the shared buffer.
/// Also updates the RMS level over the most recent 1024 mono samples.
fn handle_input_f32(data: &[f32], channels: usize, shared: &Arc<Mutex<Shared>>) {
    let mut s = shared.lock().unwrap();
    if !s.recording {
        return;
    }

    // Mix down to mono
    let mono: Vec<f32> = if channels == 1 {
        data.to_vec()
    } else {
        data.chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    };

    s.buffer.extend_from_slice(&mono);

    // Compute RMS over last 1024 samples
    let window_size = 1024_usize;
    let start = s.buffer.len().saturating_sub(window_size);
    let window = &s.buffer[start..];
    let rms = (window.iter().map(|&x| x * x).sum::<f32>() / window.len() as f32).sqrt();
    // Soft-clip to 0–1
    s.level = (rms * 4.0).min(1.0);
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
