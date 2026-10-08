// Voice & meeting capture.
//
// Two sources, one abstraction:
//   * Mic — cross-platform capture via `cpal` (default input device), sampled
//     as f32 mono and stored in a ring into a WAV file on stop.
//   * System audio (Windows loopback) — WASAPI `eRender` loopback so a Zoom
//     meeting speaks through it only while the user has Meeting Mode on, with a
//     clear recording indicator in the island (never hidden).
//
// A captured audio clip is written to `%TEMP%\coucou_voice\<ts>.wav`, then sent
// to the configured Whisper-compatible endpoint (`/v1/audio/transcriptions`).
// The endpoint base URL + API key are provider-scoped settings like the chat.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Marker sound name for "recording started" — no real WAV, just a UI blip.
pub const REC_SAMPLE_RATE: u32 = 16_000;

pub enum Source {
    Mic,
    System,
}

/// Where an active recording is parked while its thread runs.
pub struct RecordState {
    pub start: SystemTime,
    pub samples: Mutex<Vec<f32>>,
}

impl RecordState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            start: SystemTime::now(),
            samples: Mutex::new(Vec::new()),
        })
    }
}

fn clip_dir() -> PathBuf {
    std::env::temp_dir().join("coucou_voice")
}

/// Starts capture on a worker thread and returns a guard with a `stop()`
/// method that flushes the clip to WAV and returns its path.
pub struct CaptureHandle {
    pub state: Arc<RecordState>,
    pub source: Source,
    stop_flag: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CaptureHandle {
    pub fn stop(self) -> Result<PathBuf, String> {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(j) = self.join {
            let _ = j.join();
        }
        let dir = clip_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = dir.join(format!("{stamp}.wav"));
        let samples = self.state.samples.lock().unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: REC_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).map_err(|e| e.to_string())?;
        for &s in samples.iter() {
            let v = if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) };
            writer.write_sample((v * i16::MAX as f32) as i16).map_err(|e| e.to_string())?;
        }
        writer.finalize().map_err(|e| e.to_string())?;
        drop(samples);
        Ok(path)
    }
}

/// Opens the default input (mic) and streams f32 mono into `state.samples`.
fn run_mic(state: Arc<RecordState>, stop: Arc<AtomicBool>) -> Result<(), String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "No microphone found on this machine.".to_string())?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("cannot read mic config: {e}"))?;
    let rate = config.sample_rate().0 as u32;
    let channels = config.channels() as usize;

    let stream = device
        .build_input_stream(
            &config.into(),
            move |data: &[f32], _| {
                let mut out = state.samples.lock().unwrap();
                // Resample via linear step from the native rate to 16 kHz mono.
                let step = if rate == 0 { 1.0 } else { rate as f64 / REC_SAMPLE_RATE as f64 };
                let mut acc = 0.0f64;
                for &s in data {
                    if step >= 1.0 && acc >= 1.0 {
                        let idx = acc.floor() as usize * channels;
                        if idx < data.len().max(channels) {
                            let v = data
                                .get(idx.min(data.len() - 1))
                                .copied()
                                .unwrap_or(s);
                            out.push(v);
                        }
                        acc -= 1.0;
                    } else {
                        acc += step;
                    }
                }
            },
            |_| {},
            None,
        )
        .map_err(|e| format!("cannot open the mic stream: {e}"))?;

    stream.play().map_err(|e| format!("cannot start the mic: {e}"))?;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(30));
    }
    Ok(())
}

/// Starts mic capture. When `stop()` is called the clip is written to WAV.
pub fn start_capture(source: Source) -> Result<CaptureHandle, String> {
    let state = RecordState::new();
    let stop_flag = Arc::new(AtomicBool::new(false));

    match source {
        Source::Mic => {
            let s = state.clone();
            let st = stop_flag.clone();
            let join = std::thread::spawn(move || {
                let _ = run_mic(s, st);
            });
            Ok(CaptureHandle {
                state,
                source,
                stop_flag,
                join: Some(join),
            })
        }
        Source::System => Err(
            "System audio capture (meeting loopback) is not available in this build yet — use Microphone instead."
                .to_string(),
        ),
    }
}

/// Turns the user-supplied base URL into the transcriptions endpoint:
///   * `…/v1/audio/transcriptions`        → as-is
///   * `…/v1`  (or root like api.openai.com) → `…/v1/audio/transcriptions`
///   * anything weird → still mapped, but the final URL is echoed in errors so
///     a wrong setting is obvious instead of a mystery 404/405.
fn transcriptions_url(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    if b.is_empty() {
        return "https://api.openai.com/v1/audio/transcriptions".to_string();
    }
    if b.ends_with("/v1/audio/transcriptions") {
        return b.to_string();
    }
    if b.ends_with("/audio/transcriptions") {
        return b.to_string();
    }
    if b.ends_with("/v1") {
        return format!("{b}/audio/transcriptions");
    }
    format!("{b}/v1/audio/transcriptions")
}

/// Sends a WAV clip to the configured Whisper-compatible endpoint and returns
/// the transcript text.
pub async fn transcribe(
    path: &std::path::Path,
    base_url: &str,
    api_key: &str,
) -> Result<String, String> {
    let url = transcriptions_url(base_url);
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read the clip: {e}"))?;
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "clip.wav".to_string());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .text("response_format", "json")
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes)
                .file_name(file_name)
                .mime_str("audio/wav")
                .map_err(|e| e.to_string())?,
        );

    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Speech-to-text network error: {e}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "Speech-to-text {status} (tried {url}): {text}"
        ));
    }
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Bad speech-to-text response: {e}"))?;
    let transcript = json
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "The speech-to-text endpoint returned no text.".to_string())?;
    if transcript.trim().is_empty() {
        return Err("No speech was heard — try again, closer to the mic.".to_string());
    }
    Ok(transcript)
}