use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

/// A track being faded out, with the gain it had when its fade began.
struct Outgoing {
    player: Player,
    from: f32,
}

/// Wraps rodio playback on a shared mixer so tracks can crossfade. Every track
/// gets a fresh `Player`: a reused one keeps reporting its previous track's
/// position until the new source produces its first samples, which made the
/// fade ramp read as already finished and hard-cut instead. The
/// `MixerDeviceSink` handle must be kept alive for the whole program,
/// otherwise audio stops.
pub struct Audio {
    handle: MixerDeviceSink,
    /// Player holding the (incoming / current) track.
    cur: Player,
    /// Players fading out. More than one when a new fade starts mid-fade.
    outgoing: Vec<Outgoing>,
    fade_dur: f32,
    /// User-set master volume; per-player gain is scaled by the fade ramp.
    master_volume: f32,
    crossfade_secs: f32,
    current: Option<PathBuf>,
    duration: Duration,
    started: bool,
    /// The current track's stream state, when it plays from a URL.
    stream: Option<Arc<crate::stream::Shared>>,
    /// Speed for podcast episodes.
    speed: f32,
}

impl Audio {
    pub fn new(crossfade_secs: f32) -> Result<Self> {
        let handle = DeviceSinkBuilder::open_default_sink()
            .context("failed to open default audio output device")?;
        let cur = Player::connect_new(handle.mixer());
        let master_volume = 0.8;
        cur.set_volume(master_volume);
        Ok(Self {
            handle,
            cur,
            outgoing: Vec::new(),
            fade_dur: 0.0,
            master_volume,
            crossfade_secs,
            current: None,
            duration: Duration::ZERO,
            started: false,
            stream: None,
            speed: 1.0,
        })
    }

    pub fn current_path(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    pub fn crossfade_secs(&self) -> f32 {
        self.crossfade_secs
    }

    pub fn set_crossfade(&mut self, secs: f32) {
        self.crossfade_secs = secs.max(0.0);
    }

    fn decode(&self, path: &Path, length_secs: u64) -> Result<(Box<dyn Source + Send>, Option<Arc<crate::stream::Shared>>)> {
        if crate::stream::is_url(path) {
            let shared = Arc::new(crate::stream::Shared::default());
            // Only episodes (which have a length) are sped up.
            if length_secs > 0 {
                shared.set_speed(self.speed);
            }
            let url = path.to_string_lossy();
            let src = crate::stream::UrlSource::open(&url, length_secs > 0, shared.clone())
                .with_context(|| format!("starting ffmpeg for {url} (is ffmpeg installed?)"))?;
            return Ok((Box::new(src), Some(shared)));
        }
        Ok((Self::decode_file(path)?, None))
    }

    fn decode_file(path: &Path) -> Result<Box<dyn Source + Send>> {
        if crate::opus::is_ogg(path)
            && let Ok(src) = crate::opus::OpusSource::open(path)
        {
            return Ok(Box::new(src));
        }
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let dec = Decoder::try_from(file).with_context(|| format!("decoding {}", path.display()))?;
        Ok(Box::new(dec))
    }

    fn fading(&self) -> bool {
        !self.outgoing.is_empty()
    }

    /// A new player on the shared mixer, matching the current pause state.
    fn new_player(&self, volume: f32) -> Player {
        let p = Player::connect_new(self.handle.mixer());
        p.set_volume(volume);
        if self.current.is_some() && self.cur.is_paused() {
            p.pause();
        }
        p
    }

    /// Hard-start a track, cancelling any crossfade.
    pub fn play(&mut self, path: &Path, length_secs: u64) -> Result<()> {
        let (decoder, stream) = self.decode(path, length_secs)?;
        self.stream = stream;
        // Dropping a Player stops whatever it was playing.
        self.outgoing.clear();
        self.cur = Player::connect_new(self.handle.mixer());
        self.cur.set_volume(self.master_volume);
        self.cur.append(decoder);
        self.current = Some(path.to_path_buf());
        self.duration = Duration::from_secs(length_secs);
        self.started = true;
        Ok(())
    }

    /// Begin crossfading into `path` on a fresh player. Returns false if the
    /// track can't be decoded (caller should not commit the track change).
    pub fn crossfade_to(&mut self, path: &Path, length_secs: u64) -> bool {
        let Ok((decoder, stream)) = self.decode(path, length_secs) else {
            return false;
        };
        self.stream = stream;
        let incoming = self.new_player(0.0);
        incoming.append(decoder);
        let prev = std::mem::replace(&mut self.cur, incoming);
        // Fade out from wherever it currently is, so skipping again mid-fade
        // doesn't make the half-faded-in track jump back to full volume.
        let from = prev.volume();
        self.outgoing.push(Outgoing { player: prev, from });
        self.fade_dur = self.crossfade_secs.max(0.1);
        self.current = Some(path.to_path_buf());
        self.duration = Duration::from_secs(length_secs);
        self.started = true;
        true
    }

    /// Should an automatic crossfade into the next track begin now? True when
    /// the current track is within `crossfade_secs` of its end.
    pub fn should_start_crossfade(&self) -> bool {
        if self.crossfade_secs <= 0.0 || self.fading() || self.current.is_none() {
            return false;
        }
        let dur = self.duration().as_secs_f32();
        if dur <= self.crossfade_secs + 0.5 {
            return false; // too short to fade meaningfully
        }
        let pos = self.position().as_secs_f32();
        pos >= 0.5 && (dur - pos) <= self.crossfade_secs
    }

    /// Per-tick crossfade ramp. Fade progress is derived from the incoming
    /// player's position, so it naturally freezes while paused and doesn't
    /// start until the new track's audio actually does.
    pub fn update(&mut self) {
        if !self.fading() {
            return;
        }
        let t = (self.cur.get_pos().as_secs_f32() / self.fade_dur).clamp(0.0, 1.0);
        // Finalize early if the incoming track ended before the fade completed.
        if t >= 1.0 || self.cur.empty() {
            self.outgoing.clear();
            self.cur.set_volume(self.master_volume);
            return;
        }
        self.cur.set_volume(self.master_volume * t);
        self.outgoing.retain(|o| !o.player.empty());
        for o in &self.outgoing {
            o.player.set_volume(o.from.min(self.master_volume) * (1.0 - t));
        }
    }

    pub fn toggle_pause(&mut self) {
        if self.current.is_none() {
            return;
        }
        let paused = self.cur.is_paused();
        for p in std::iter::once(&self.cur).chain(self.outgoing.iter().map(|o| &o.player)) {
            if paused {
                p.play();
            } else {
                p.pause();
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.current.is_some() && !self.cur.is_paused()
    }

    /// True when a track finished naturally (no crossfade in progress).
    pub fn finished(&self) -> bool {
        self.started && self.current.is_some() && !self.fading() && self.cur.empty()
    }

    pub fn position(&self) -> Duration {
        match &self.stream {
            Some(s) => s.position(),
            None => self.cur.get_pos(),
        }
    }

    /// A live stream's current song, as the station names it.
    pub fn stream_title(&self) -> Option<String> {
        self.stream.as_ref().and_then(|s| s.title.lock().ok()?.clone())
    }

    /// Podcast speed; applies to the playing episode at once.
    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed.clamp(0.5, 3.0);
        if let Some(s) = &self.stream
            && !self.duration.is_zero()
        {
            s.set_speed(self.speed);
        }
    }

    pub fn duration(&self) -> Duration {
        // An episode whose feed gave no length: ffmpeg's, once it knows.
        if self.duration <= Duration::from_secs(1)
            && let Some(d) = self.stream.as_ref().and_then(|s| s.duration())
        {
            return d;
        }
        self.duration
    }

    /// The real length, once a track that started streaming has finished.
    pub fn set_duration(&mut self, secs: u64) {
        self.duration = Duration::from_secs(secs);
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
        if !self.fading() {
            self.cur.set_volume(self.master_volume);
        }
    }

    pub fn seek_to(&mut self, pos: Duration) -> bool {
        if self.current.is_none() {
            return false;
        }
        let target = pos.min(self.duration());
        self.cur.try_seek(target).is_ok()
    }

    pub fn seek_relative(&mut self, delta: i64) -> bool {
        let cur = self.position().as_secs_f64();
        let target = (cur + delta as f64).max(0.0);
        self.seek_to(Duration::from_secs_f64(target))
    }

    pub fn seek_fraction(&mut self, frac: f64) -> bool {
        if self.duration().is_zero() {
            return false;
        }
        let secs = self.duration().as_secs_f64() * frac.clamp(0.0, 1.0);
        self.seek_to(Duration::from_secs_f64(secs))
    }

    pub fn stop(&mut self) {
        self.outgoing.clear();
        self.cur.stop();
        self.current = None;
        self.started = false;
        self.duration = Duration::ZERO;
        self.stream = None;
    }
}
