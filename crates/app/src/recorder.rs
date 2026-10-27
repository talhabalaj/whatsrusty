//! Voice-note recording from the default microphone.
//!
//! Capture uses `AVAudioEngine` the way the user's Said app does: a tap on the input node in
//! the microphone's own hardware format, without voice processing. Nothing about the device
//! is reconfigured, so audio that's already playing keeps playing at its volume and the
//! recording has no gap at the start. (cpal rewrote the device's physical format first, which
//! interrupted playback and dropped the first part of the input.) Building an input node can
//! block for up to a second, so one engine is kept prepared between recordings.
//!
//! The composer reads a shared snapshot (timer, level meter) while it renders and takes the
//! samples when the note is sent.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_avf_audio::{AVAudioEngine, AVAudioPCMBuffer, AVAudioTime};

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
    /// Recent loudest bar (dBFS), decaying: the meter is relative to it.
    peak_db: f64,
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
                let db = 20. * rms.max(1e-6).log10();
                self.levels.push(meter_level(db, &mut self.peak_db));
                (self.level_sum, self.level_n) = (0., 0);
            }
        }
    }
}

/// The meter's reference never drops below this, so room noise stays small.
const FLOOR_DB: f64 = -60.;

/// One meter bar (0.04..=1) for a slice at `db` dBFS. Relative to the recent peak, like
/// Messages: macOS sets the built-in microphone's gain as it likes (it can drop ~45 dB a
/// second into a recording), so absolute levels would flatten to dots while someone talks.
fn meter_level(db: f64, peak_db: &mut f64) -> f32 {
    /// The peak fades by this much per bar (~33 dB/s), so a loud start doesn't mute the rest.
    const DECAY_DB: f64 = 2.;
    /// Below this the reference stops following, so room noise stays small.
    /// Dynamic range shown on the meter.
    const RANGE_DB: f64 = 30.;
    *peak_db = db.max(*peak_db - DECAY_DB).max(FLOOR_DB);
    ((db - (*peak_db - RANGE_DB)) / RANGE_DB).clamp(0.04, 1.) as f32
}

pub struct Recorder {
    captured: Arc<Mutex<Captured>>,
    stop: Arc<AtomicBool>,
    started: Instant,
    engine: Option<Engine>,
}

/// An `AVAudioEngine` that may move between threads: Apple documents the engine as safe to
/// start, stop and tear down from any thread, and only one owner uses it at a time.
struct Engine(Retained<AVAudioEngine>);
unsafe impl Send for Engine {}

/// The prepared engine waiting for the next recording.
static WARM: Mutex<Option<Engine>> = Mutex::new(None);
static WARMING: AtomicBool = AtomicBool::new(false);

/// Create and prepare an engine (input node bound, not running: the microphone stays off).
fn prepared_engine() -> Engine {
    unsafe {
        let engine = AVAudioEngine::new();
        let input = engine.inputNode();
        let _ = input.inputFormatForBus(0);
        engine.prepare();
        Engine(engine)
    }
}

/// Prepare an engine in the background for the next recording, if the microphone is allowed.
pub fn warm_up() {
    if mic_access() != MicAccess::Granted || std::env::var_os("WA_DEBUG_FAKE_MIC").is_some() {
        return;
    }
    if WARM.lock().is_ok_and(|w| w.is_some()) || WARMING.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = std::thread::Builder::new().name("mic-warmup".into()).spawn(|| {
        let engine = prepared_engine();
        if let Ok(mut warm) = WARM.lock()
            && warm.is_none()
        {
            *warm = Some(engine);
        }
        WARMING.store(false, Ordering::Release);
    });
}

impl Recorder {
    /// Start recording right away. Failures (no microphone, access denied) show up in
    /// [`Recorder::error`]. `WA_DEBUG_FAKE_MIC` records a synthetic voice instead.
    pub fn start() -> Self {
        let captured = Arc::new(Mutex::new(Captured { peak_db: FLOOR_DB, ..Captured::default() }));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, s) = (captured.clone(), stop.clone());
        if std::env::var_os("WA_DEBUG_FAKE_MIC").is_some() {
            let _ = std::thread::Builder::new().name("fake-mic".into()).spawn(move || fake_mic(&c, &s));
            return Self { captured, stop, started: Instant::now(), engine: None };
        }
        let warm = WARM.lock().ok().and_then(|mut w| w.take());
        let engine = warm.unwrap_or_else(prepared_engine);
        let engine = match capture(&engine, &captured) {
            Ok(()) => Some(engine),
            Err(e) => {
                log::warn!("recording failed: {e:#}");
                if let Ok(mut c) = captured.lock() {
                    c.error = Some(e.to_string());
                }
                None
            }
        };
        Self { captured, stop, started: Instant::now(), engine }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The last `n` meter levels (0..=1), oldest first.
    pub fn levels(&self, n: usize) -> Vec<f32> {
        let Ok(c) = self.captured.lock() else { return Vec::new() };
        c.levels[c.levels.len().saturating_sub(n)..].to_vec()
    }

    /// Keep recording when another app reconfigures the microphone (a call app, or one
    /// enabling voice processing switches the built-in mic to its raw array): AVAudioEngine
    /// stops on such a change, so tap it again and carry on. Called from the UI tick.
    pub fn keep_alive(&mut self) {
        let Some(Engine(engine)) = &self.engine else { return };
        if self.stop.load(Ordering::Relaxed) || unsafe { engine.isRunning() } {
            return;
        }
        log::info!("microphone was reconfigured; restarting the recording tap");
        unsafe { engine.inputNode().removeTapOnBus(0) };
        if let Err(e) = capture(self.engine.as_ref().expect("checked above"), &self.captured) {
            log::warn!("restarting the recording failed: {e:#}");
            if let Ok(mut c) = self.captured.lock() {
                c.error = Some(e.to_string());
            }
        }
    }

    pub fn error(&self) -> Option<String> {
        self.captured.lock().ok().and_then(|c| c.error.clone())
    }

    /// Stop and hand over what was recorded: mono samples and their rate.
    pub fn finish(mut self) -> (Vec<i16>, u32) {
        self.shut_down();
        let Ok(mut c) = self.captured.lock() else { return (Vec::new(), 0) };
        (std::mem::take(&mut c.samples), c.rate)
    }

    /// Stop the microphone (the tap delivers nothing after this) and prepare the next engine.
    fn shut_down(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(Engine(engine)) = self.engine.take() {
            unsafe {
                engine.inputNode().removeTapOnBus(0);
                engine.stop();
            }
            warm_up();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// Tap the engine's input node in the microphone's hardware format and start it.
fn capture(engine: &Engine, captured: &Arc<Mutex<Captured>>) -> anyhow::Result<()> {
    let Engine(engine) = engine;
    unsafe {
        let input = engine.inputNode();
        let format = input.inputFormatForBus(0);
        let rate = format.sampleRate().round() as u32;
        anyhow::ensure!(rate > 0 && format.channelCount() > 0, "No microphone found");
        // The note keeps the rate it started with; a restarted tap at another rate is resampled.
        let note_rate = {
            let mut c = captured.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            if c.rate == 0 {
                c.rate = rate;
            }
            c.rate
        };
        let step = f64::from(rate) / f64::from(note_rate);
        let sink = captured.clone();
        let tap = block2::RcBlock::new(move |buffer: NonNull<AVAudioPCMBuffer>, _: NonNull<AVAudioTime>| {
            let buffer = buffer.as_ref();
            let frames = buffer.frameLength() as usize;
            let data = buffer.floatChannelData();
            if frames == 0 || data.is_null() {
                return;
            }
            let channels = (buffer.format().channelCount() as usize).max(1);
            let stride = buffer.stride().max(1);
            // Non-interleaved float channels: downmix to mono.
            let sample = |i: usize| (0..channels).map(|ch| *(*data.add(ch)).as_ptr().add(i * stride)).sum::<f32>() / channels as f32;
            // While another app reconfigures the device, the tap delivers exact digital zeros
            // (a real microphone never does): leave them out instead of recording dead air.
            if (0..frames).all(|i| sample(i) == 0.) {
                return;
            }
            let out = (frames as f64 / step) as usize;
            let mono = (0..out).map(|j| sample(((j as f64 * step) as usize).min(frames - 1)));
            if let Ok(mut c) = sink.lock() {
                c.push(mono);
            }
        });
        input.installTapOnBus_bufferSize_format_block(0, 4096, Some(&format), &*tap as *const _ as *mut _);
        if let Err(e) = engine.startAndReturnError() {
            input.removeTapOnBus(0);
            anyhow::bail!("{}", e.localizedDescription());
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_recovers_after_the_microphone_gain_drops() {
        let mut peak = FLOOR_DB;
        // Loud for 0.7 s, then macOS turns the gain down ~45 dB while the speech goes on.
        let loud: Vec<f32> = (0..12).map(|_| meter_level(-3., &mut peak)).collect();
        assert!(loud.iter().all(|l| *l > 0.9), "{loud:?}");
        let after: Vec<f32> = (0..60).map(|i| meter_level(if i % 4 == 0 { -62. } else { -48. }, &mut peak)).collect();
        // Within ~1.5 s (25 bars) speech shows as tall bars again, pauses as shorter ones.
        for (i, l) in after.iter().enumerate().skip(25) {
            if i % 4 == 0 {
                assert!(*l < 0.65, "pause {i}: {after:?}");
            } else {
                assert!(*l > 0.75, "speech {i}: {after:?}");
            }
        }
    }

    #[test]
    fn room_noise_stays_small() {
        let mut peak = FLOOR_DB;
        let noise: Vec<f32> = (0..50).map(|_| meter_level(-85., &mut peak)).collect();
        assert!(noise.iter().all(|l| *l < 0.2), "{noise:?}");
    }
}
