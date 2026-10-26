//! Voice-note playback. One clip plays at a time, like Messages.
//!
//! Opening the CoreAudio output device and decoding can stall (device switches, slow
//! disks), so all of it runs on a dedicated audio thread. The UI sends commands and reads
//! a small shared state snapshot while rendering.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::{App, Global};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};

enum Cmd {
    Toggle { id: String, path: PathBuf },
    Stop,
}

#[derive(Default, Clone)]
struct Snapshot {
    id: Option<String>,
    position: Duration,
    playing: bool,
}

pub struct AudioPlayer {
    commands: flume::Sender<Cmd>,
    state: Arc<Mutex<Snapshot>>,
    /// When the UI last sent a command, so views keep repainting until the worker reports.
    last_command: Option<std::time::Instant>,
}

impl Global for AudioPlayer {}

impl AudioPlayer {
    pub fn init(cx: &mut App) {
        let (tx, rx) = flume::unbounded();
        let state = Arc::new(Mutex::new(Snapshot::default()));
        let worker_state = state.clone();
        std::thread::Builder::new()
            .name("audio".into())
            .spawn(move || worker(rx, worker_state))
            .expect("spawn audio thread");
        cx.set_global(AudioPlayer { commands: tx, state, last_command: None });
    }

    /// Play `path` for `message_id`, or pause/resume it if it's already loaded.
    pub fn toggle(cx: &mut App, message_id: &str, path: &Path) {
        if cx.has_global::<AudioPlayer>() {
            let this = cx.global_mut::<AudioPlayer>();
            this.last_command = Some(std::time::Instant::now());
            let _ = this.commands.send(Cmd::Toggle { id: message_id.to_string(), path: path.to_path_buf() });
        }
    }

    pub fn stop(cx: &mut App) {
        if let Some(this) = cx.try_global::<AudioPlayer>() {
            let _ = this.commands.send(Cmd::Stop);
        }
    }

    /// Something is playing, or a command was just sent: keep animating.
    pub fn busy(cx: &App) -> bool {
        let Some(this) = cx.try_global::<AudioPlayer>() else { return false };
        this.last_command.is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
            || this.state.lock().is_ok_and(|s| s.playing)
    }

    /// Playback state of `message_id`: `Some((position, playing))` if it's the loaded clip.
    pub fn state(cx: &App, message_id: &str) -> Option<(Duration, bool)> {
        let this = cx.try_global::<AudioPlayer>()?;
        let s = this.state.lock().ok()?;
        (s.id.as_deref() == Some(message_id)).then_some((s.position, s.playing))
    }
}

fn worker(rx: flume::Receiver<Cmd>, state: Arc<Mutex<Snapshot>>) {
    let mut sink: Option<MixerDeviceSink> = None;
    let mut player: Option<Player> = None;
    let mut current: Option<String> = None;
    let publish = |id: &Option<String>, player: &Option<Player>| {
        let snap = match player {
            Some(p) if !p.empty() => Snapshot { id: id.clone(), position: p.get_pos(), playing: !p.is_paused() },
            _ => Snapshot::default(),
        };
        if let Ok(mut s) = state.lock() {
            *s = snap;
        }
    };
    loop {
        // Tick while something is loaded so the UI's progress bar moves.
        let cmd = if player.is_some() {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(cmd) => Some(cmd),
                Err(flume::RecvTimeoutError::Timeout) => None,
                Err(flume::RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => return,
            }
        };
        match cmd {
            Some(Cmd::Toggle { id, path }) => {
                if current.as_deref() == Some(id.as_str())
                    && let Some(p) = player.as_ref().filter(|p| !p.empty())
                {
                    if p.is_paused() { p.play() } else { p.pause() }
                } else {
                    if let Some(old) = player.take() {
                        old.stop();
                    }
                    match start(&mut sink, &path) {
                        Ok(p) => {
                            player = Some(p);
                            current = Some(id);
                        }
                        Err(e) => {
                            log::warn!("voice playback failed: {e:#}");
                            current = None;
                        }
                    }
                }
            }
            Some(Cmd::Stop) => {
                if let Some(old) = player.take() {
                    old.stop();
                }
                current = None;
            }
            None => {}
        }
        if player.as_ref().is_some_and(|p| p.empty()) {
            player = None;
            current = None;
        }
        publish(&current, &player);
    }
}

/// Open the default output. rodio asks for ~50 ms of buffer (2048 frames at 48 kHz),
/// which some devices refuse (e.g. Bluetooth headsets allow only 15..=960), so fall
/// back to smaller fixed sizes and finally the OS default.
fn open_sink() -> anyhow::Result<MixerDeviceSink> {
    use rodio::cpal::BufferSize;
    let first = match DeviceSinkBuilder::open_default_sink() {
        Ok(sink) => return Ok(sink),
        Err(e) => e,
    };
    for size in [BufferSize::Fixed(960), BufferSize::Fixed(512), BufferSize::Fixed(256), BufferSize::Default] {
        let attempt = DeviceSinkBuilder::from_default_device().and_then(|b| b.with_buffer_size(size).open_stream());
        match attempt {
            Ok(sink) => {
                log::info!("audio output opened with buffer {size:?} (default failed: {first})");
                return Ok(sink);
            }
            Err(e) => log::debug!("audio output with buffer {size:?} failed: {e}"),
        }
    }
    Err(first.into())
}

fn start(sink: &mut Option<MixerDeviceSink>, path: &Path) -> anyhow::Result<Player> {
    if sink.is_none() {
        let mut s = open_sink()?;
        s.log_on_drop(false);
        *sink = Some(s);
    }
    let player = Player::connect_new(sink.as_ref().expect("opened above").mixer());
    player.append(Decoder::try_from(File::open(path)?)?);
    Ok(player)
}
