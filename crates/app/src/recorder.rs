//! Voice-note recording from the default microphone.
//!
//! The input stream isn't `Send` on macOS, so it lives on its own thread; the composer
//! reads a shared snapshot (timer, level meter) while it renders and takes the samples
//! when the note is sent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rodio::cpal;
use rodio::cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};

/// One level-meter bar per this much audio.
const LEVEL_EVERY: Duration = Duration::from_millis(60);
/// WhatsApp's limit is far longer, but notes are held in memory: stop at 15 minutes.
pub const MAX_LENGTH: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
struct Captured {
    /// Mono.
    samples: Vec<i16>,
    rate: u32,
    /// Loudness per `LEVEL_EVERY`, 0..=1.
    levels: Vec<f32>,
    /// Accumulator for the current level bar.
    level_sum: f64,
    level_n: usize,
    error: Option<String>,
}

impl Captured {
    fn push(&mut self, mono: impl Iterator<Item = f32>) {
        let per_level = (self.rate as f64 * LEVEL_EVERY.as_secs_f64()) as usize;
        for s in mono {
            let s = s.clamp(-1., 1.);
            self.samples.push((s * f32::from(i16::MAX)) as i16);
            self.level_sum += f64::from(s * s);
            self.level_n += 1;
            if self.level_n >= per_level.max(1) {
                let rms = (self.level_sum / self.level_n as f64).sqrt();
                // Speech sits around -30..-10 dBFS: spread that over the meter.
                let db = 20. * rms.max(1e-6).log10();
                self.levels.push(((db + 50.) / 45.).clamp(0.04, 1.) as f32);
                (self.level_sum, self.level_n) = (0., 0);
            }
        }
    }
}

pub struct Recorder {
    captured: Arc<Mutex<Captured>>,
    stop: Arc<AtomicBool>,
    started: Instant,
}

impl Recorder {
    /// Start recording right away. Failures (no microphone, access denied) show up in
    /// [`Recorder::error`]. `WA_DEBUG_FAKE_MIC` records a synthetic voice instead.
    pub fn start() -> Self {
        let captured = Arc::new(Mutex::new(Captured::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, s) = (captured.clone(), stop.clone());
        let fake = std::env::var_os("WA_DEBUG_FAKE_MIC").is_some();
        let spawned = std::thread::Builder::new().name("recorder".into()).spawn(move || {
            let result = if fake { fake_mic(&c, &s) } else { capture(&c, &s) };
            if let Err(e) = result {
                log::warn!("recording failed: {e:#}");
                if let Ok(mut c) = c.lock() {
                    c.error = Some(e.to_string());
                }
            }
        });
        if let Err(e) = spawned
            && let Ok(mut c) = captured.lock()
        {
            c.error = Some(e.to_string());
        }
        Self { captured, stop, started: Instant::now() }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The last `n` meter levels (0..=1), oldest first.
    pub fn levels(&self, n: usize) -> Vec<f32> {
        let Ok(c) = self.captured.lock() else { return Vec::new() };
        c.levels[c.levels.len().saturating_sub(n)..].to_vec()
    }

    pub fn error(&self) -> Option<String> {
        self.captured.lock().ok().and_then(|c| c.error.clone())
    }

    /// Stop and hand over what was recorded: mono samples and their rate.
    pub fn finish(self) -> (Vec<i16>, u32) {
        self.stop.store(true, Ordering::Relaxed);
        let Ok(mut c) = self.captured.lock() else { return (Vec::new(), 0) };
        (std::mem::take(&mut c.samples), c.rate)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn capture(captured: &Arc<Mutex<Captured>>, stop: &AtomicBool) -> anyhow::Result<()> {
    let device = cpal::default_host().default_input_device().ok_or_else(|| anyhow::anyhow!("No microphone found"))?;
    let supported = device.default_input_config()?;
    let channels = usize::from(supported.channels()).max(1);
    captured.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.rate = supported.sample_rate();
    let config: cpal::StreamConfig = supported.config();
    let on_error = |e: cpal::Error| log::warn!("microphone: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build::<f32>(&device, config, channels, captured.clone(), on_error)?,
        cpal::SampleFormat::I16 => build::<i16>(&device, config, channels, captured.clone(), on_error)?,
        cpal::SampleFormat::I32 => build::<i32>(&device, config, channels, captured.clone(), on_error)?,
        other => anyhow::bail!("unsupported microphone format {other:?}"),
    };
    stream.play()?;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    captured: Arc<Mutex<Captured>>,
    on_error: impl FnMut(cpal::Error) + Send + 'static,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // Downmix to mono.
            let mono = data.chunks(channels).map(|frame| frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() / frame.len() as f32);
            if let Ok(mut c) = captured.lock() {
                c.push(mono);
            }
        },
        on_error,
        None,
    )?;
    Ok(stream)
}

/// A wobbling tone with syllable-like pauses, in real time (mock mode / screenshots).
fn fake_mic(captured: &Arc<Mutex<Captured>>, stop: &AtomicBool) -> anyhow::Result<()> {
    const RATE: u32 = 16_000;
    captured.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.rate = RATE;
    let mut t = 0usize;
    while !stop.load(Ordering::Relaxed) {
        let chunk: Vec<f32> = (t..t + 320)
            .map(|i| {
                let secs = i as f32 / RATE as f32;
                let envelope = ((secs * 3.1).sin() * (secs * 0.7).sin()).abs();
                (secs * 220. * std::f32::consts::TAU).sin() * 0.5 * envelope
            })
            .collect();
        t += chunk.len();
        if let Ok(mut c) = captured.lock() {
            c.push(chunk.into_iter());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Microphone access as macOS sees it (AVFoundation). A denied app gets silence, not an
/// error, from the input stream, so check before recording.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MicAccess {
    Granted,
    Denied,
    /// Never asked: [`request_access`] shows the system prompt.
    Undetermined,
}

pub fn mic_access() -> MicAccess {
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
    // Test hooks: a fake microphone is always allowed; WA_DEBUG_MIC=denied refuses.
    if std::env::var("WA_DEBUG_MIC").is_ok_and(|v| v == "denied") {
        return MicAccess::Denied;
    }
    if std::env::var_os("WA_DEBUG_FAKE_MIC").is_some() {
        return MicAccess::Granted;
    }
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else { return MicAccess::Granted };
    match unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) } {
        AVAuthorizationStatus::Authorized => MicAccess::Granted,
        AVAuthorizationStatus::NotDetermined => MicAccess::Undetermined,
        _ => MicAccess::Denied,
    }
}

/// Show the system prompt; the answer arrives on the returned channel (AVFoundation
/// answers on an arbitrary thread).
pub fn request_access() -> flume::Receiver<bool> {
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
    let (tx, rx) = flume::bounded(1);
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        let _ = tx.send(true);
        return rx;
    };
    let block = block2::RcBlock::new(move |granted: objc2::runtime::Bool| {
        let _ = tx.send(granted.as_bool());
    });
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(audio, &block) };
    rx
}

/// System Settings › Privacy & Security › Microphone.
pub fn open_privacy_settings() {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
        .spawn();
}
