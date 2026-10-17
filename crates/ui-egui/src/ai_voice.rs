//! "Word" AI voice input: record the default microphone, then transcribe with Groq's free
//! whisper-large-v3-turbo. Desktop only. Needs GROQ_API_KEY.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

/// A recording in progress (and, after `stop`, its transcription).
pub struct Listening {
    stop: Arc<AtomicBool>,
    rx: Receiver<Result<String, String>>,
    /// Stop was pressed: the audio is being transcribed.
    pub stopping: bool,
}

impl Listening {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.stopping = true;
    }

    /// The transcript (or error) once it is ready.
    pub fn poll(&self) -> Option<Result<String, String>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("the recording stopped unexpectedly".into())),
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub fn start() -> Result<Listening, String> {
    Err("voice input works in the desktop app".into())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn start() -> Result<Listening, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("word-voice".into())
        .spawn(move || {
            let _ = tx.send(native::run(&flag));
        })
        .map_err(|e| e.to_string())?;
    Ok(Listening { stop, rx, stopping: false })
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Longest recording (seconds); it stops by itself after this.
    const MAX_SECONDS: usize = 120;

    pub fn run(stop: &AtomicBool) -> Result<String, String> {
        let (samples, rate) = record(stop)?;
        // Under a quarter of a second: nothing was said.
        if samples.len() < (rate as usize) / 4 {
            return Ok(String::new());
        }
        let pcm = to_16k(&samples, rate);
        transcribe(&wav(&pcm, 16_000))
    }

    /// Mono-mix `frames` (interleaved) into the buffer, up to `cap` samples.
    fn push(buf: &Mutex<Vec<f32>>, channels: usize, cap: usize, frames: &[f32]) {
        if let Ok(mut b) = buf.lock() {
            for frame in frames.chunks(channels) {
                if b.len() >= cap {
                    break;
                }
                b.push(frame.iter().sum::<f32>() / frame.len() as f32);
            }
        }
    }

    fn record(stop: &AtomicBool) -> Result<(Vec<f32>, u32), String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("no microphone found")?;
        let supported = device.default_input_config().map_err(|e| format!("microphone: {e}"))?;
        let rate = supported.sample_rate().0;
        let channels = usize::from(supported.channels()).max(1);
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let cap = rate as usize * MAX_SECONDS;
        let buf = Arc::new(Mutex::new(Vec::<f32>::new()));
        let err_fn = |e: cpal::StreamError| log::warn!("microphone: {e}");

        let stream = match format {
            cpal::SampleFormat::F32 => {
                let b = buf.clone();
                device.build_input_stream(&config, move |d: &[f32], _: &cpal::InputCallbackInfo| push(&b, channels, cap, d), err_fn, None)
            }
            cpal::SampleFormat::I16 => {
                let b = buf.clone();
                device.build_input_stream(
                    &config,
                    move |d: &[i16], _: &cpal::InputCallbackInfo| {
                        let v: Vec<f32> = d.iter().map(|s| f32::from(*s) / 32768.0).collect();
                        push(&b, channels, cap, &v)
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let b = buf.clone();
                device.build_input_stream(
                    &config,
                    move |d: &[u16], _: &cpal::InputCallbackInfo| {
                        let v: Vec<f32> = d.iter().map(|s| (f32::from(*s) - 32768.0) / 32768.0).collect();
                        push(&b, channels, cap, &v)
                    },
                    err_fn,
                    None,
                )
            }
            other => return Err(format!("unsupported microphone format {other:?}")),
        }
        .map_err(|e| format!("microphone: {e}"))?;
        stream.play().map_err(|e| format!("microphone: {e}"))?;

        let started = Instant::now();
        while !stop.load(Ordering::Relaxed) && started.elapsed() < Duration::from_secs(MAX_SECONDS as u64) {
            std::thread::sleep(Duration::from_millis(30));
        }
        drop(stream);
        let samples = buf.lock().map(|b| b.clone()).unwrap_or_default();
        Ok((samples, rate))
    }

    /// Box-filter down to 16 kHz, 16-bit: small uploads, and what speech models want.
    fn to_16k(samples: &[f32], rate: u32) -> Vec<i16> {
        if rate == 0 {
            return Vec::new();
        }
        let ratio = rate as f32 / 16_000.0;
        let n = (samples.len() as f32 / ratio) as usize;
        (0..n)
            .map(|i| {
                let a = ((i as f32) * ratio) as usize;
                let b = ((((i + 1) as f32) * ratio) as usize).max(a + 1).min(samples.len());
                let v = samples.get(a..b).filter(|s| !s.is_empty()).map(|s| s.iter().sum::<f32>() / s.len() as f32).unwrap_or(0.0);
                (v.clamp(-1.0, 1.0) * 32767.0) as i16
            })
            .collect()
    }

    /// A mono 16-bit PCM WAV file.
    fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut v = Vec::with_capacity(44 + samples.len() * 2);
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 2).to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    fn field(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }

    fn transcribe(wav: &[u8]) -> Result<String, String> {
        let key = crate::ai_net::key(crate::ai_net::Provider::Groq).ok_or("GROQ_API_KEY is not set (voice uses Groq's free Whisper)")?;
        let boundary = "----wordcraft-word-ai-7e3a9c";
        let mut body: Vec<u8> = Vec::with_capacity(wav.len() + 600);
        field(&mut body, boundary, "model", "whisper-large-v3-turbo");
        field(&mut body, boundary, "response_format", "json");
        field(&mut body, boundary, "temperature", "0");
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(wav);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(60))).http_status_as_error(false).build();
        let agent = ureq::Agent::new_with_config(config);
        let content_type = format!("multipart/form-data; boundary={boundary}");
        let auth = format!("Bearer {key}");
        let mut resp = agent
            .post("https://api.groq.com/openai/v1/audio/transcriptions")
            .header("authorization", auth.as_str())
            .header("content-type", content_type.as_str())
            .send(body.as_slice())
            .map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}: {}", text.chars().take(300).collect::<String>()));
        }
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        Ok(v.get("text").and_then(serde_json::Value::as_str).unwrap_or("").trim().to_string())
    }
}