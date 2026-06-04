use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};

/// An in-progress crossfade: `out` is the player being faded out; the current
/// player (`Audio::cur`) is the one being faded in.
#[derive(Clone, Copy)]
struct Fade {
    out: usize,
    dur: f32,
}

/// Wraps rodio playback with two players on a shared mixer so tracks can
/// crossfade. The `MixerDeviceSink` handle must be kept alive for the whole
/// program, otherwise audio stops.
pub struct Audio {
    _handle: MixerDeviceSink,
    players: [Player; 2],
    /// Index of the player holding the (incoming / current) track.
    cur: usize,
    /// User-set master volume; per-player gain is scaled by the fade ramp.
    master_volume: f32,
    crossfade_secs: f32,
    current: Option<PathBuf>,
    duration: Duration,
    started: bool,
    fade: Option<Fade>,
}

impl Audio {
    pub fn new(crossfade_secs: f32) -> Result<Self> {
        let handle = DeviceSinkBuilder::open_default_sink()
            .context("failed to open default audio output device")?;
        let players = [
            Player::connect_new(handle.mixer()),
            Player::connect_new(handle.mixer()),
        ];
        let master_volume = 0.8;
        players[0].set_volume(master_volume);
        players[1].set_volume(0.0);
        Ok(Self {
            _handle: handle,
            players,
            cur: 0,
            master_volume,
            crossfade_secs,
            current: None,
            duration: Duration::ZERO,
            started: false,
            fade: None,
        })
    }

    pub fn crossfade_secs(&self) -> f32 {
        self.crossfade_secs
    }

    pub fn set_crossfade(&mut self, secs: f32) {
        self.crossfade_secs = secs.max(0.0);
    }

    fn decode(path: &Path) -> Result<Decoder<std::io::BufReader<File>>> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        Decoder::try_from(file).with_context(|| format!("decoding {}", path.display()))
    }

    /// Hard-start a track on the current player, cancelling any crossfade.
    pub fn play(&mut self, path: &Path, length_secs: u64) -> Result<()> {
        let decoder = Self::decode(path)?;
        let other = 1 - self.cur;
        self.players[other].stop();
        self.fade = None;
        let p = &self.players[self.cur];
        p.clear();
        p.append(decoder);
        p.set_volume(self.master_volume);
        p.play();
        self.current = Some(path.to_path_buf());
        self.duration = Duration::from_secs(length_secs);
        self.started = true;
        Ok(())
    }

    /// Begin crossfading into `path` on the idle player. Returns false if the
    /// track can't be decoded (caller should not commit the track change).
    pub fn crossfade_to(&mut self, path: &Path, length_secs: u64) -> bool {
        let Ok(decoder) = Self::decode(path) else {
            return false;
        };
        let other = 1 - self.cur;
        self.players[other].clear();
        self.players[other].set_volume(0.0);
        self.players[other].append(decoder);
        self.players[other].play();
        self.fade = Some(Fade {
            out: self.cur,
            dur: self.crossfade_secs.max(0.1),
        });
        self.cur = other;
        self.current = Some(path.to_path_buf());
        self.duration = Duration::from_secs(length_secs);
        self.started = true;
        true
    }

    /// Should an automatic crossfade into the next track begin now? True when
    /// the current track is within `crossfade_secs` of its end.
    pub fn should_start_crossfade(&self) -> bool {
        if self.crossfade_secs <= 0.0 || self.fade.is_some() || self.current.is_none() {
            return false;
        }
        let dur = self.duration.as_secs_f32();
        if dur <= self.crossfade_secs + 0.5 {
            return false; // too short to fade meaningfully
        }
        let pos = self.position().as_secs_f32();
        pos >= 0.5 && (dur - pos) <= self.crossfade_secs
    }

    /// Per-tick crossfade ramp. Fade progress is derived from the incoming
    /// player's position, so it naturally freezes while paused.
    pub fn update(&mut self) {
        let Some(fade) = self.fade else { return };
        let incoming = &self.players[self.cur];
        // Finalize early if the incoming track ended before the fade completed.
        let t = (incoming.get_pos().as_secs_f32() / fade.dur).clamp(0.0, 1.0);
        if t >= 1.0 || incoming.empty() {
            self.players[fade.out].stop();
            self.players[self.cur].set_volume(self.master_volume);
            self.fade = None;
            return;
        }
        self.players[self.cur].set_volume(self.master_volume * t);
        self.players[fade.out].set_volume(self.master_volume * (1.0 - t));
    }

    pub fn toggle_pause(&mut self) {
        if self.current.is_none() {
            return;
        }
        let paused = self.players[self.cur].is_paused();
        for p in &self.players {
            if paused {
                p.play();
            } else {
                p.pause();
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.current.is_some() && !self.players[self.cur].is_paused()
    }

    /// True when a track finished naturally (no crossfade in progress).
    pub fn finished(&self) -> bool {
        self.started && self.current.is_some() && self.fade.is_none() && self.players[self.cur].empty()
    }

    pub fn position(&self) -> Duration {
        self.players[self.cur].get_pos()
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn volume(&self) -> f32 {
        self.master_volume
    }

    pub fn change_volume(&mut self, delta: f32) {
        self.set_volume(self.master_volume + delta);
    }

    pub fn set_volume(&mut self, v: f32) {
        self.master_volume = v.clamp(0.0, 1.0);
        // During a fade, update() reapplies the ramped gain next tick.
        if self.fade.is_none() {
            self.players[self.cur].set_volume(self.master_volume);
        }
    }

    pub fn seek_to(&mut self, pos: Duration) -> bool {
        if self.current.is_none() {
            return false;
        }
        let target = pos.min(self.duration);
        self.players[self.cur].try_seek(target).is_ok()
    }

    pub fn seek_relative(&mut self, delta: i64) -> bool {
        let cur = self.position().as_secs_f64();
        let target = (cur + delta as f64).max(0.0);
        self.seek_to(Duration::from_secs_f64(target))
    }

    pub fn seek_fraction(&mut self, frac: f64) -> bool {
        if self.duration.is_zero() {
            return false;
        }
        let secs = self.duration.as_secs_f64() * frac.clamp(0.0, 1.0);
        self.seek_to(Duration::from_secs_f64(secs))
    }

    pub fn stop(&mut self) {
        for p in &self.players {
            p.stop();
        }
        self.current = None;
        self.started = false;
        self.duration = Duration::ZERO;
        self.fade = None;
    }
}
