//! MPRIS: publishes the current track on the session bus and takes media
//! commands from it, so `playerctl`, media keys, status bars and phone remotes
//! all work. Commands arrive on the same channel as the `--play-pause` flags.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
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

pub struct Mpris {
    controls: MediaControls,
    track: Option<(PathBuf, Duration)>,
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
        Some(Self {
            controls,
            track: None,
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
        let track = now.map(|t| (t.path.clone(), audio.duration()));
        if track != self.track {
            let _ = self.controls.set_metadata(match now {
                Some(t) => MediaMetadata {
                    title: Some(&t.title),
                    artist: Some(&t.track_artist).filter(|a| !a.is_empty()).map(String::as_str),
                    album: Some(&t.album).filter(|a| !a.is_empty()).map(String::as_str),
                    duration: Some(audio.duration()).filter(|d| !d.is_zero()),
                    cover_url: None,
                },
                None => MediaMetadata::default(),
            });
            self.track = track;
            self.published = Instant::now() - Duration::from_secs(60); // republish position below
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
