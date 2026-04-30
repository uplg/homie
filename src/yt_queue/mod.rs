//! `!yt`-driven music queue with pause-other-players coordination.
//!
//! Architecture:
//!
//! - One background tokio task owns the rodio `Player` and the pending
//!   `VecDeque`. The state we expose for `!queue` lives behind a `RwLock`
//!   so chat handlers can read it cheaply.
//! - Every chat command sends a [`Command`] over an unbounded mpsc and
//!   the worker reacts on its main `select!` loop.
//! - When a track ends, rodio invokes the `EmptyCallback` we appended
//!   right after the audio source. The callback wakes the worker via a
//!   second mpsc, which then advances the queue.
//! - The first time we start a viewer track in a fresh "session" we pause
//!   Music.app / Spotify (whichever is playing) and remember which one;
//!   when the queue drains we resume the same app.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::{RwLock, mpsc};

pub mod download;
pub mod macos;

/// Tunable bounds. Defaults are reasonable for a home stream.
#[derive(Debug, Clone)]
pub struct Config {
    pub max_duration_secs: u32,
    pub max_queue_size: usize,
    pub initial_volume_percent: u8,
    /// Optional substring matched against output device names (case
    /// insensitive). When `None`, the system default device is used.
    /// Useful to point the bot at a virtual device like `BlackHole` so OBS
    /// can capture its audio independently of the system mix.
    pub audio_device: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_duration_secs: 600,
            max_queue_size: 20,
            initial_volume_percent: 80,
            audio_device: None,
        }
    }
}

/// What a chat reply should say after `enqueue`.
#[derive(Debug, Clone)]
pub enum EnqueueOutcome {
    StartingNow { title: String },
    Queued { title: String, position: usize },
    Rejected(String),
}

/// One viewer-submitted track, post-validation.
#[derive(Debug, Clone)]
pub struct Track {
    pub url: String,
    pub title: String,
    pub duration_secs: u32,
    pub requested_by: String,
}

/// Public, lockless-read view of the queue.
#[derive(Debug, Clone, Default)]
pub struct PublicState {
    pub current: Option<Track>,
    pub pending: Vec<Track>,
    pub volume_percent: u8,
}

#[derive(Debug)]
enum Command {
    Enqueue(Track),
    Skip,
    SetVolume(u8),
    Shutdown,
}

pub struct YtQueue {
    cmd_tx: mpsc::UnboundedSender<Command>,
    state: Arc<RwLock<PublicState>>,
    max_queue_size: usize,
    available: Arc<AtomicBool>,
}

impl YtQueue {
    /// Build the queue and start its worker task.
    ///
    /// `cache_dir` is where audio downloads land. If yt-dlp is missing,
    /// `enqueue` will reject submissions with a friendly message but the
    /// rest of the bot keeps working.
    pub async fn start(cache_dir: PathBuf, config: Config) -> Self {
        let available = match download::check_yt_dlp_available().await {
            Ok(version) => {
                tracing::info!(version = %version, "yt-dlp available");
                true
            }
            Err(err) => {
                tracing::warn!(error = %err, "yt-dlp not available — !yt commands will be rejected");
                false
            }
        };

        // Best-effort check of nowplaying-cli; logs its own warning.
        let _ = macos::check_available().await;

        // Log the available output devices so the operator knows what to put
        // in TWITCHY_AUDIO_DEVICE.
        log_available_output_devices();

        let state = Arc::new(RwLock::new(PublicState {
            volume_percent: config.initial_volume_percent.min(100),
            ..Default::default()
        }));

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        // Pre-create cache dir so download tasks don't race on it.
        if let Err(err) = tokio::fs::create_dir_all(&cache_dir).await {
            tracing::warn!(error = %err, dir = %cache_dir.display(), "could not create yt cache dir");
        }

        let max_queue_size = config.max_queue_size;
        let initial_volume = config.initial_volume_percent.min(100);
        let worker = Worker {
            cmd_rx,
            state: state.clone(),
            cache_dir,
            pending: VecDeque::new(),
            pressed_play_pause: false,
            current_volume: f32::from(initial_volume) / 100.0,
            audio_device: config.audio_device,
        };
        tokio::spawn(worker.run());

        Self {
            cmd_tx,
            state,
            max_queue_size,
            available: Arc::new(AtomicBool::new(available)),
        }
    }

    /// Probe a URL and push to the queue. Returns the user-facing outcome.
    pub async fn enqueue(&self, url: &str, requested_by: &str) -> EnqueueOutcome {
        if !self.available.load(Ordering::Relaxed) {
            return EnqueueOutcome::Rejected(
                "yt-dlp is not installed on the bot host (brew install yt-dlp)".to_string(),
            );
        }

        let pending_count = self.state.read().await.pending.len();
        if pending_count >= self.max_queue_size {
            return EnqueueOutcome::Rejected(format!(
                "queue is full ({} pending)",
                self.max_queue_size
            ));
        }

        let meta = match download::probe(url).await {
            Ok(meta) => meta,
            Err(err) => return EnqueueOutcome::Rejected(format!("probe failed: {err}")),
        };

        if meta.is_live {
            return EnqueueOutcome::Rejected("live streams are not allowed".to_string());
        }

        let track = Track {
            url: url.to_string(),
            title: meta.title,
            duration_secs: meta.duration_secs,
            requested_by: requested_by.to_string(),
        };

        // Snapshot what the worker will do next.
        let starting_now = self.state.read().await.current.is_none() && pending_count == 0;
        let title_clone = track.title.clone();

        if self.cmd_tx.send(Command::Enqueue(track)).is_err() {
            return EnqueueOutcome::Rejected("music worker has stopped".to_string());
        }

        if starting_now {
            EnqueueOutcome::StartingNow { title: title_clone }
        } else {
            EnqueueOutcome::Queued {
                title: title_clone,
                position: pending_count + 1,
            }
        }
    }

    pub fn skip(&self) {
        let _ = self.cmd_tx.send(Command::Skip);
    }

    #[must_use]
    pub fn set_volume(&self, percent: u8) -> u8 {
        let clamped = percent.min(100);
        let _ = self.cmd_tx.send(Command::SetVolume(clamped));
        clamped
    }

    pub async fn snapshot(&self) -> PublicState {
        self.state.read().await.clone()
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
    }
}

/// The worker task. Owns rodio so we never serialize that bit across awaits.
struct Worker {
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    state: Arc<RwLock<PublicState>>,
    cache_dir: PathBuf,
    pending: VecDeque<Track>,
    /// Set to `true` once we've sent the play/pause toggle for this queue
    /// session, so we know to send it again when the queue drains.
    pressed_play_pause: bool,
    current_volume: f32,
    audio_device: Option<String>,
}

/// Holds the rodio resources. Built lazily on the first track so the bot
/// never opens an audio device unless someone actually queues a song.
struct Engine {
    _stream: rodio::MixerDeviceSink,
    player: rodio::Player,
}

impl Engine {
    fn new(volume: f32, device_hint: Option<&str>) -> std::result::Result<Self, String> {
        let stream = if let Some(hint) = device_hint {
            let needle = hint.to_ascii_lowercase();
            let device = list_output_devices()
                .into_iter()
                .find(|(name, _)| name.to_ascii_lowercase().contains(&needle))
                .map(|(_, dev)| dev)
                .ok_or_else(|| format!("no output device matched '{hint}'"))?;
            let device_name = cpal_device_name(&device);
            tracing::info!(device = %device_name, "routing viewer audio to selected device");
            rodio::DeviceSinkBuilder::from_device(device)
                .map_err(|err| format!("build sink for '{hint}': {err}"))?
                .open_sink_or_fallback()
                .map_err(|err| format!("open sink for '{hint}': {err}"))?
        } else {
            tracing::info!("routing viewer audio to system default output");
            rodio::DeviceSinkBuilder::open_default_sink()
                .map_err(|err| format!("open default audio device: {err}"))?
        };

        let player = rodio::Player::connect_new(stream.mixer());
        player.set_volume(volume);
        Ok(Self {
            _stream: stream,
            player,
        })
    }
}

/// Wrap [`cpal::traits::DeviceTrait::name`] (deprecated in newer cpal in
/// favour of `description()` / `id()` but still the simplest path to a
/// printable string) and silence the deprecation locally.
#[allow(deprecated)]
fn cpal_device_name(device: &cpal::Device) -> String {
    use cpal::traits::DeviceTrait;
    device.name().unwrap_or_else(|_| "<unknown>".to_string())
}

/// Enumerate macOS output devices as `(name, device)` pairs.
fn list_output_devices() -> Vec<(String, cpal::Device)> {
    use cpal::traits::HostTrait;
    cpal::default_host()
        .output_devices()
        .map(|iter| {
            iter.map(|d| {
                let name = cpal_device_name(&d);
                (name, d)
            })
            .collect()
        })
        .unwrap_or_default()
}

/// Log the available macOS output devices at INFO once on startup so the
/// user knows exactly what string to put in `TWITCHY_AUDIO_DEVICE`.
fn log_available_output_devices() {
    let names: Vec<String> = list_output_devices().into_iter().map(|(n, _)| n).collect();
    if names.is_empty() {
        tracing::info!("no output audio devices visible to cpal");
    } else {
        tracing::info!(
            devices = ?names,
            "available output audio devices (set TWITCHY_AUDIO_DEVICE to a substring to pick one)"
        );
    }
}

impl Worker {
    async fn run(mut self) {
        let mut engine: Option<Engine> = None;
        let (end_tx, mut end_rx) = mpsc::unbounded_channel::<()>();

        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Command::Enqueue(track) => {
                            self.pending.push_back(track);
                            self.refresh_pending_state().await;
                            self.maybe_advance(&mut engine, &end_tx).await;
                        }
                        Command::Skip => {
                            if let Some(engine) = engine.as_ref() {
                                engine.player.skip_one();
                            }
                            // The end callback fires via skip_one's emitted
                            // EmptyCallback, so we let the normal end path run.
                        }
                        Command::SetVolume(percent) => {
                            self.current_volume = f32::from(percent) / 100.0;
                            if let Some(engine) = engine.as_ref() {
                                engine.player.set_volume(self.current_volume);
                            }
                            self.state.write().await.volume_percent = percent;
                        }
                        Command::Shutdown => {
                            if let Some(engine) = engine.as_ref() {
                                engine.player.stop();
                            }
                            if self.pressed_play_pause {
                                macos::resume().await;
                                self.pressed_play_pause = false;
                            }
                            break;
                        }
                    }
                }
                Some(()) = end_rx.recv() => {
                    self.state.write().await.current = None;
                    self.maybe_advance(&mut engine, &end_tx).await;
                }
            }
        }

        // Drain temp files on the way out.
        let _ = tokio::fs::remove_dir_all(&self.cache_dir).await;
    }

    async fn refresh_pending_state(&self) {
        let mut state = self.state.write().await;
        state.pending = self.pending.iter().cloned().collect();
    }

    async fn maybe_advance(
        &mut self,
        engine: &mut Option<Engine>,
        end_tx: &mpsc::UnboundedSender<()>,
    ) {
        // If something is still playing, do nothing.
        {
            let state = self.state.read().await;
            if state.current.is_some() {
                return;
            }
        }

        let Some(track) = self.pending.pop_front() else {
            // Queue drained — only resume if WE paused the user's audio earlier.
            // Otherwise we'd kick a paused/stopped Now Playing source into
            // playing, which is exactly the bug a previous version had.
            if self.pressed_play_pause {
                tracing::info!("queue empty, resuming user's audio");
                macos::resume().await;
                self.pressed_play_pause = false;
            }
            self.refresh_pending_state().await;
            return;
        };

        // First viewer track of this session: pause whatever macOS is currently
        // playing — but ONLY if it's actually playing. `pause_if_playing()`
        // queries `nowplaying-cli get playbackRate` first and skips the toggle
        // when nothing is playing, so a paused user does not get unpaused.
        if !self.pressed_play_pause && macos::pause_if_playing().await {
            self.pressed_play_pause = true;
            tokio::time::sleep(macos::PAUSE_TO_PLAY_DELAY).await;
        }

        // Make sure we have an audio engine.
        if engine.is_none() {
            match Engine::new(self.current_volume, self.audio_device.as_deref()) {
                Ok(e) => *engine = Some(e),
                Err(err) => {
                    tracing::error!(error = %err, "could not open audio device, dropping track");
                    self.refresh_pending_state().await;
                    return;
                }
            }
        }
        let Some(engine_ref) = engine.as_ref() else {
            return;
        };

        // Download to disk.
        let dir = self.cache_dir.join(format!(
            "{}-{}",
            track.requested_by.replace(['/', '\\'], "_"),
            sanitize_id(&track.url)
        ));
        let path = match download::download_audio(&track.url, &dir).await {
            Ok(p) => p,
            Err(err) => {
                tracing::error!(error = %err, url = %track.url, "yt-dlp download failed, skipping");
                self.refresh_pending_state().await;
                return;
            }
        };

        // Decode and queue. yt-dlp transcodes to mp3 (`-x --audio-format mp3`)
        // so we always feed the symphonia mp3 decoder.
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(err) => {
                tracing::error!(error = %err, path = %path.display(), "open downloaded file");
                let _ = tokio::fs::remove_dir_all(&dir).await;
                self.refresh_pending_state().await;
                return;
            }
        };
        let decoder = match rodio::decoder::DecoderBuilder::new()
            .with_data(std::io::BufReader::new(file))
            .with_hint("mp3")
            .build()
        {
            Ok(d) => d,
            Err(err) => {
                tracing::error!(error = %err, path = %path.display(), "decode downloaded file");
                let _ = tokio::fs::remove_dir_all(&dir).await;
                self.refresh_pending_state().await;
                return;
            }
        };

        engine_ref.player.append(decoder);
        let end_tx = end_tx.clone();
        let dir_to_clean = dir;
        engine_ref
            .player
            .append(rodio::source::EmptyCallback::new(Box::new(move || {
                // Fire-and-forget: the channel might be closed during shutdown.
                let _ = end_tx.send(());
                let _ = std::fs::remove_dir_all(&dir_to_clean);
            })));

        tracing::info!(
            user = %track.requested_by,
            title = %track.title,
            duration = track.duration_secs,
            "now playing",
        );

        let mut state = self.state.write().await;
        state.current = Some(track);
        state.pending = self.pending.iter().cloned().collect();
    }
}

/// Make a `YouTube` URL safe for use as a directory name.
fn sanitize_id(url: &str) -> String {
    url.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(40)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_id_is_filesystem_safe() {
        assert_eq!(
            sanitize_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            "httpswwwyoutubecomwatchvdQw4w9WgXcQ"
        );
        assert!(!sanitize_id("a/b\\c").contains(['/', '\\']));
        assert!(sanitize_id("a".repeat(100).as_str()).len() <= 40);
    }

    #[test]
    fn config_defaults_are_reasonable() {
        let cfg = Config::default();
        assert_eq!(cfg.max_duration_secs, 600);
        assert_eq!(cfg.max_queue_size, 20);
        assert!(cfg.initial_volume_percent <= 100);
    }
}
