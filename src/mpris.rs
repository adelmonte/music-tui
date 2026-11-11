//! MPRIS: publishes the current track on the session bus and takes media
//! commands from it, so `playerctl`, media keys, status bars and phone remotes
//! all work. Commands arrive on the same channel as the `--play-pause` flags.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};

use crate::app::App;
use crate::ipc::Cmd;

/// How far a bare "seek forward/backward" request moves.
const SEEK_STEP: i64 = 5;

#[derive(PartialEq, Clone, Copy)]
enum State {
    Stopped,
    Paused,
    Playing,
}

/// A track's cover, written out as a file other programs can read.
struct Cover {
    track: PathBuf,
    url: Option<String>,
}

pub struct Mpris {
    controls: MediaControls,
    track: Option<(PathBuf, Duration, Option<String>)>,
    /// Cover of the current track, once the worker has produced it.
    cover: Option<Cover>,
    cover_tx: Sender<Cover>,
    cover_rx: Receiver<Cover>,
    state: State,
    volume: f32,
    /// Position as last published, to notice seeks.
    position: Duration,
    published: Instant,
}

impl Mpris {
    /// `None` when there is no session bus; the player works without it.
    pub fn new(tx: Sender<Cmd>) -> Option<Self> {
        let mut controls = MediaControls::new(PlatformConfig {
            dbus_name: "music_tui",
            display_name: "music-tui",
            hwnd: None,
        })
        .ok()?;
        controls
            .attach(move |event| {
                let cmd = match event {
                    MediaControlEvent::Play => Cmd::Play,
                    MediaControlEvent::Pause | MediaControlEvent::Stop => Cmd::Pause,
                    MediaControlEvent::Toggle => Cmd::PlayPause,
                    MediaControlEvent::Next => Cmd::Next,
                    MediaControlEvent::Previous => Cmd::Previous,
                    MediaControlEvent::Seek(dir) => Cmd::SeekBy(signed(dir, SEEK_STEP)),
                    MediaControlEvent::SeekBy(dir, by) => Cmd::SeekBy(signed(dir, by.as_secs() as i64)),
                    MediaControlEvent::SetPosition(MediaPosition(pos)) => Cmd::SeekTo(pos),
                    MediaControlEvent::SetVolume(v) => Cmd::SetVolume(v),
                    _ => return,
                };
                let _ = tx.send(cmd);
            })
            .ok()?;
        let (cover_tx, cover_rx) = channel();
        Some(Self {
            controls,
            track: None,
            cover: None,
            cover_tx,
            cover_rx,
            state: State::Stopped,
            volume: -1.0,
            position: Duration::ZERO,
            published: Instant::now(),
        })
    }

    /// Publish whatever changed since the last call. Cheap when nothing did.
    pub fn update(&mut self, app: &App) {
        let audio = &app.audio;
        let state = match (audio.current_path().is_some(), audio.is_playing()) {
            (false, _) => State::Stopped,
            (true, false) => State::Paused,
            (true, true) => State::Playing,
        };

        let now = app.now.and_then(|i| app.queue.get(i)).filter(|_| state != State::Stopped);

        // Covers are read (or fetched) on a worker, then published when ready.
        while let Ok(c) = self.cover_rx.try_recv() {
            if now.is_some_and(|t| t.path == c.track) {
                self.cover = Some(c);
            }
        }
        if let Some(t) = now
            && self.track.as_ref().is_none_or(|(p, ..)| *p != t.path)
        {
            self.cover = None;
            let (tx, path) = (self.cover_tx.clone(), t.path.clone());
            std::thread::spawn(move || {
                let url = export_cover(&path);
                let _ = tx.send(Cover { track: path, url });
            });
        }
        let cover = self.cover.as_ref().and_then(|c| c.url.clone());

        let track = now.map(|t| (t.path.clone(), audio.duration(), cover.clone()));
        if track != self.track {
            let _ = self.controls.set_metadata(match now {
                Some(t) => MediaMetadata {
                    title: Some(&t.title),
                    artist: Some(&t.track_artist).filter(|a| !a.is_empty()).map(String::as_str),
                    album: Some(&t.album).filter(|a| !a.is_empty()).map(String::as_str),
                    duration: Some(audio.duration()).filter(|d| !d.is_zero()),
                    cover_url: cover.as_deref(),
                },
                None => MediaMetadata::default(),
            });
            let new_song = self.track.as_ref().map(|t| &t.0) != track.as_ref().map(|t| &t.0);
            self.track = track;
            if new_song {
                self.published = Instant::now() - Duration::from_secs(60); // republish position below
            }
        }

        // Clients extrapolate the position while playing, but ones that poll
        // read back what was last published: refresh it every second, and at
        // once on a seek or a state change.
        let position = audio.position();
        let expected = if self.state == State::Playing { self.position + self.published.elapsed() } else { self.position };
        let jumped = position.abs_diff(expected) > Duration::from_millis(1500);
        let stale = state == State::Playing && self.published.elapsed() >= Duration::from_secs(1);
        if state != self.state || jumped || stale {
            let progress = Some(MediaPosition(position));
            let _ = self.controls.set_playback(match state {
                State::Stopped => MediaPlayback::Stopped,
                State::Paused => MediaPlayback::Paused { progress },
                State::Playing => MediaPlayback::Playing { progress },
            });
            self.state = state;
            self.position = position;
            self.published = Instant::now();
        }

        let volume = audio.volume();
        if (volume - self.volume).abs() > 0.001 {
            let _ = self.controls.set_volume(volume as f64);
            self.volume = volume;
        }
    }
}

fn signed(dir: SeekDirection, secs: i64) -> i64 {
    match dir {
        SeekDirection::Forward => secs,
        SeekDirection::Backward => -secs,
    }
}

/// Write the track's cover where other programs can read it and return its
/// `file://` URL. One file is kept; the name changes per track so clients
/// notice the change.
fn export_cover(track: &Path) -> Option<String> {
    let bytes = crate::spotify::cover_bytes(track)?;
    let dir = crate::config::art_cache_dir().join("mpris");
    std::fs::create_dir_all(&dir).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    track.hash(&mut h);
    let ext = if bytes.starts_with(b"\x89PNG") { "png" } else { "jpg" };
    let file = dir.join(format!("{:016x}.{ext}", h.finish()));
    if let Ok(old) = std::fs::read_dir(&dir) {
        for e in old.flatten() {
            if e.path() != file {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    std::fs::write(&file, bytes).ok()?;
    Some(format!("file://{}", file.display()))
}
