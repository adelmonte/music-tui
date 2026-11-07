//! Audio from a URL (internet radio, podcast episodes), decoded by ffmpeg
//! into 48 kHz stereo floats. A reader thread hands the samples over in
//! chunks, so the audio thread never waits on the network: while nothing
//! has arrived it plays silence. Seeking and speed changes restart ffmpeg
//! at the new position (`-ss` seeks with HTTP ranges, `atempo` changes speed
//! without changing pitch). Live streams' song titles (ICY `StreamTitle`)
//! are read from ffmpeg's log.

use std::io::{BufRead, BufReader, Read};
use std::num::NonZero;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// Floats per chunk handed to the audio thread (~43 ms).
const CHUNK: usize = 4096;
/// Chunks buffered ahead (~5 s).
const AHEAD: usize = 128;

/// Cover images for URL tracks (a station's logo, a show's artwork), by
/// the track's path, recorded wherever those tracks are made.
fn art_map() -> &'static Mutex<std::collections::HashMap<std::path::PathBuf, String>> {
    static MAP: std::sync::OnceLock<Mutex<std::collections::HashMap<std::path::PathBuf, String>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}

pub fn set_art(path: &Path, url: &str) {
    if !url.is_empty()
        && let Ok(mut m) = art_map().lock()
    {
        m.insert(path.to_path_buf(), url.to_string());
    }
}

pub fn art(path: &Path) -> Option<String> {
    art_map().lock().ok()?.get(path).cloned()
}

/// True if the track's path is a URL rather than a file.
pub fn is_url(path: &Path) -> bool {
    path.to_str().is_some_and(|s| s.starts_with("http://") || s.starts_with("https://"))
}

/// State shared between a playing stream and the app.
#[derive(Default)]
pub struct Shared {
    /// The live stream's current song, as the station names it.
    pub title: Mutex<Option<String>>,
    /// Position in the episode, in ms (sped-up playback counts as more).
    pos_ms: AtomicU64,
    /// Requested speed (f32 bits); 0 means 1.0.
    speed: AtomicU32,
    /// The length ffmpeg reports, in ms (0 until known).
    duration_ms: AtomicU64,
    /// Where the URL's redirects end (podcast links pass through trackers),
    /// so seeking doesn't follow them all again.
    resolved: Mutex<Option<String>>,
}

impl Shared {
    pub fn position(&self) -> Duration {
        Duration::from_millis(self.pos_ms.load(Ordering::Relaxed))
    }

    pub fn duration(&self) -> Option<Duration> {
        Some(Duration::from_millis(self.duration_ms.load(Ordering::Relaxed))).filter(|d| !d.is_zero())
    }

    pub fn set_speed(&self, speed: f32) {
        self.speed.store(speed.to_bits(), Ordering::Relaxed);
    }

    fn speed(&self) -> f32 {
        match self.speed.load(Ordering::Relaxed) {
            0 => 1.0,
            b => f32::from_bits(b),
        }
    }
}

pub struct UrlSource {
    url: String,
    seekable: bool,
    speed: f32,
    child: Option<Child>,
    rx: Receiver<Vec<f32>>,
    buf: Vec<f32>,
    at: usize,
    /// Where the current ffmpeg run started, and samples (per channel)
    /// played since.
    base: Duration,
    played: u64,
    shared: Arc<Shared>,
}

impl UrlSource {
    /// `seekable`: a file with a length (an episode), not a live stream.
    pub fn open(url: &str, seekable: bool, shared: Arc<Shared>) -> std::io::Result<Self> {
        let speed = if seekable { shared.speed() } else { 1.0 };
        let (child, rx) = spawn(url, Duration::ZERO, speed, &shared)?;
        if seekable {
            let (url, shared) = (url.to_string(), shared.clone());
            std::thread::spawn(move || {
                use ureq::ResponseExt;
                if let Ok(resp) = ureq::get(&url).header("Range", "bytes=0-0").call()
                    && let Ok(mut r) = shared.resolved.lock()
                {
                    *r = Some(resp.get_uri().to_string());
                }
            });
        }
        Ok(Self {
            url: url.to_string(),
            seekable,
            speed,
            child: Some(child),
            rx,
            buf: Vec::new(),
            at: 0,
            base: Duration::ZERO,
            played: 0,
            shared,
        })
    }

    fn restart(&mut self, at: Duration, speed: f32) -> std::io::Result<()> {
        self.stop();
        let url = self.shared.resolved.lock().ok().and_then(|r| r.clone()).unwrap_or_else(|| self.url.clone());
        let (child, rx) = spawn(&url, at, speed, &self.shared)?;
        self.child = Some(child);
        self.rx = rx;
        self.buf.clear();
        self.at = 0;
        self.base = at;
        self.played = 0;
        self.speed = speed;
        self.shared.pos_ms.store(at.as_millis() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn content_pos(&self) -> Duration {
        self.base + Duration::from_secs_f64(self.played as f64 / RATE as f64 * self.speed as f64)
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for UrlSource {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn(url: &str, at: Duration, speed: f32, shared: &Arc<Shared>) -> std::io::Result<(Child, Receiver<Vec<f32>>)> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-nostdin", "-hide_banner", "-nostats", "-loglevel", "verbose"]);
    if url.starts_with("http") {
        cmd.args([
            "-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_on_network_error", "1",
            "-reconnect_delay_max", "10", "-user_agent", "music-tui",
        ]);
    }
    if !at.is_zero() {
        cmd.args(["-ss", &format!("{:.3}", at.as_secs_f64())]);
    }
    cmd.args(["-i", url, "-vn"]);
    if (speed - 1.0).abs() > 0.01 {
        cmd.args(["-af", &format!("atempo={speed:.2}")]);
    }
    cmd.args(["-f", "f32le", "-ac", "2", "-ar", "48000", "pipe:1"]);
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (tx, rx) = sync_channel(AHEAD);
    let mut out = child.stdout.take().expect("piped");
    std::thread::spawn(move || {
        let mut bytes = vec![0u8; CHUNK * 4];
        let mut fill = 0;
        loop {
            match out.read(&mut bytes[fill..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => fill += n,
            }
            if fill < bytes.len() {
                continue;
            }
            let chunk = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            fill = 0;
            if tx.send(chunk).is_err() {
                return;
            }
        }
        let whole = fill / 8 * 8;
        let rest = bytes[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let _ = tx.send(rest);
    });
    let err = child.stderr.take().expect("piped");
    let shared = shared.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if let Some(d) = log_duration(&line) {
                shared.duration_ms.store(d.as_millis() as u64, Ordering::Relaxed);
            }
            if let Some(title) = stream_title(&line)
                && let Ok(mut t) = shared.title.lock()
            {
                *t = (!title.is_empty()).then(|| title.to_string());
            }
        }
    });
    Ok((child, rx))
}

/// `  Duration: 00:45:12.34, start: …` (`N/A` for live streams).
fn log_duration(line: &str) -> Option<Duration> {
    let rest = line.trim_start().strip_prefix("Duration: ")?;
    let hms = rest.split(',').next()?;
    let mut secs = 0.0;
    for part in hms.split(':') {
        secs = secs * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    (secs > 0.0).then(|| Duration::from_secs_f64(secs))
}

/// The song in an ffmpeg log line: `StreamTitle     : Artist - Song` when
/// it connects, `Metadata update for StreamTitle: …` when it changes.
fn stream_title(line: &str) -> Option<&str> {
    let rest = &line[line.find("StreamTitle")? + "StreamTitle".len()..];
    Some(rest.trim_start().strip_prefix(':')?.trim())
}

impl Iterator for UrlSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.at >= self.buf.len() {
            let want = self.shared.speed();
            if self.seekable && (want - self.speed).abs() > 0.01 {
                let at = self.content_pos();
                let _ = self.restart(at, want);
            }
            match self.rx.try_recv() {
                Ok(chunk) => {
                    self.buf = chunk;
                    self.at = 0;
                }
                // Still connecting, or the network is behind.
                Err(TryRecvError::Empty) => return Some(0.0),
                Err(TryRecvError::Disconnected) => return None,
            }
            if self.buf.is_empty() {
                return Some(0.0);
            }
        }
        let s = self.buf[self.at];
        self.at += 1;
        if self.at % CHANNELS as usize == 0 {
            self.played += 1;
            if self.played % 2400 == 0 {
                self.shared.pos_ms.store(self.content_pos().as_millis() as u64, Ordering::Relaxed);
            }
        }
        Some(s)
    }
}

impl Source for UrlSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        NonZero::new(CHANNELS).expect("nonzero")
    }

    fn sample_rate(&self) -> SampleRate {
        NonZero::new(RATE).expect("nonzero")
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        if !self.seekable {
            return Err(SeekError::NotSupported { underlying_source: "live stream" });
        }
        let speed = self.speed;
        self.restart(pos, speed).map_err(|e| SeekError::Other(Arc::new(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_from_ffmpeg_lines() {
        assert_eq!(stream_title("    StreamTitle     : Dengue Fever - Uku"), Some("Dengue Fever - Uku"));
        assert_eq!(stream_title("[http @ 0x55] Metadata update for StreamTitle: A - B"), Some("A - B"));
        assert_eq!(stream_title("    icy-name        : Radio"), None);
        assert_eq!(log_duration("  Duration: 01:02:03.50, start: 0.0, bitrate: 128 kb/s"), Some(Duration::from_secs_f64(3723.5)));
        assert_eq!(log_duration("  Duration: N/A, start: 0.0"), None);
        assert!(is_url(Path::new("https://x/y.mp3")) && !is_url(Path::new("/music/a.mp3")));
    }
}

