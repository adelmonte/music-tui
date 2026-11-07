//! Ogg Opus playback (YouTube's best audio stream), which rodio's symphonia
//! backend can't decode. Decodes through libopus at 48 kHz. Seeking rescans
//! packet lengths from the start (cheap: no decoding) and decodes the last
//! 80 ms before the target to let the decoder converge, as the Opus spec
//! recommends.
//!
//! A song still downloading (see `spotify::stream_path`) is read as it grows:
//! only whole Ogg pages are exposed, and when playback catches up with the
//! download it plays silence until more arrives.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::num::NonZero;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ogg::PacketReader;
use rodio::source::SeekError;
use rodio::{ChannelCount, Sample, SampleRate, Source};

const RATE: u32 = 48_000;
/// Largest Opus packet: 120 ms at 48 kHz.
const MAX_FRAME: usize = 5760;
const PREROLL: u64 = 3840;

/// True if the file at `path` is an Ogg stream.
pub fn is_ogg(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"OggS"
}

/// A file that may still be being written. Reads stop at the end of the
/// last complete Ogg page until the download is marked finished.
struct Growing {
    file: File,
    pos: u64,
    /// Bytes safe to read.
    safe: u64,
    /// Markers whose appearance means the writer is done.
    marks: Option<[PathBuf; 2]>,
    live: Arc<AtomicBool>,
    checked: Option<Instant>,
}

impl Growing {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let marks = crate::spotify::stream_marks(&real).filter(|m| !m.iter().any(|p| p.exists()));
        let len = file.metadata()?.len();
        let mut g = Self { file, pos: 0, safe: len, live: Arc::new(AtomicBool::new(marks.is_some())), marks, checked: None };
        if g.marks.is_some() {
            g.safe = 0;
            g.refresh();
        }
        Ok(g)
    }

    /// Move `safe` up to the newest complete page (at most every 50 ms).
    fn refresh(&mut self) {
        let Some(marks) = &self.marks else { return };
        if self.checked.is_some_and(|c| c.elapsed() < Duration::from_millis(50)) {
            return;
        }
        self.checked = Some(Instant::now());
        // Checked before the length, so a finished file is read whole.
        let finished = marks.iter().any(|p| p.exists());
        let len = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        if finished {
            self.safe = len;
            self.marks = None;
            self.live.store(false, Ordering::Relaxed);
            return;
        }
        let mut head = [0u8; 27];
        let mut segs = [0u8; 255];
        loop {
            let at = self.safe;
            if self.file.read_exact_at(&mut head, at).is_err() || &head[..4] != b"OggS" {
                break;
            }
            let n = head[26] as usize;
            if self.file.read_exact_at(&mut segs[..n], at + 27).is_err() {
                break;
            }
            let size = 27 + n as u64 + segs[..n].iter().map(|&b| b as u64).sum::<u64>();
            if at + size > len {
                break;
            }
            self.safe = at + size;
        }
    }
}

impl Read for Growing {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.safe {
            self.refresh();
        }
        let n = (self.safe.saturating_sub(self.pos) as usize).min(buf.len());
        if n == 0 {
            return Ok(0);
        }
        let n = self.file.read_at(&mut buf[..n], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Growing {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let base = match to {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.safe as i64 + d,
        };
        self.pos = base.max(0) as u64;
        Ok(self.pos)
    }
}

enum Fill {
    Data,
    /// Caught up with a download in progress.
    Wait,
    End,
}

/// Samples of silence to play before checking a download again (~20 ms).
const WAIT: usize = 960;

pub struct OpusSource {
    path: PathBuf,
    reader: PacketReader<BufReader<Growing>>,
    /// Still downloading.
    live: Arc<AtomicBool>,
    /// Silent samples left before trying the download again.
    wait: usize,
    decoder: opus::Decoder,
    channels: u16,
    pre_skip: u64,
    gain: f32,
    /// Playable samples per channel (after pre-skip, before end trimming).
    total: u64,
    /// Samples per channel emitted so far (playback position).
    pos: u64,
    /// Samples per channel still to drop from the front of decoded output.
    discard: u64,
    buf: Vec<f32>,
    at: usize,
}

impl OpusSource {
    pub fn open(path: &Path) -> Result<Self> {
        let file = Growing::open(path).with_context(|| format!("opening {}", path.display()))?;
        let live = file.live.clone();
        // A download's length isn't known until it's done; it plays to the end.
        let total_granule = if live.load(Ordering::Relaxed) { 0 } else { last_granule(&file.file).unwrap_or(0) };
        let mut reader = PacketReader::new(BufReader::new(file));
        let head = reader.read_packet()?.context("empty Ogg file")?.data;
        if head.len() < 19 || &head[..8] != b"OpusHead" {
            bail!("not an Opus stream");
        }
        let channels = head[9] as u16;
        let pre_skip = u16::from_le_bytes([head[10], head[11]]) as u64;
        let gain_q8 = i16::from_le_bytes([head[16], head[17]]) as f32;
        let layout = match (head[18], channels) {
            (0, 1) => opus::Channels::Mono,
            (0, 2) => opus::Channels::Stereo,
            _ => bail!("unsupported Opus channel layout"),
        };
        // The comment header (OpusTags).
        reader.read_packet()?;
        Ok(Self {
            path: path.to_path_buf(),
            reader,
            live,
            wait: 0,
            decoder: opus::Decoder::new(RATE, layout)?,
            channels,
            pre_skip,
            gain: 10f32.powf(gain_q8 / (20.0 * 256.0)),
            total: total_granule.saturating_sub(pre_skip),
            pos: 0,
            discard: pre_skip,
            buf: Vec::new(),
            at: 0,
        })
    }

    /// Decode the next packet into `buf`.
    fn refill(&mut self) -> Fill {
        let mut out = vec![0f32; MAX_FRAME * self.channels as usize];
        loop {
            let Ok(Some(packet)) = self.reader.read_packet() else {
                return if self.live.load(Ordering::Relaxed) { Fill::Wait } else { Fill::End };
            };
            let Ok(n) = self.decoder.decode_float(&packet.data, &mut out, false) else { continue };
            let skip = (self.discard as usize).min(n);
            self.discard -= skip as u64;
            if skip == n {
                continue;
            }
            let ch = self.channels as usize;
            self.buf.clear();
            self.buf.extend(out[skip * ch..n * ch].iter().map(|s| s * self.gain));
            self.at = 0;
            return Fill::Data;
        }
    }
}

/// The granule position of the stream's last page (its total length in
/// 48 kHz samples, including pre-skip), found by scanning the file's tail.
fn last_granule(file: &File) -> Option<u64> {
    let mut f = file.try_clone().ok()?;
    let len = f.seek(SeekFrom::End(0)).ok()?;
    let start = len.saturating_sub(65_536);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    let at = tail.windows(4).rposition(|w| w == b"OggS")?;
    let g = tail.get(at + 6..at + 14)?;
    Some(u64::from_le_bytes(g.try_into().ok()?))
}

impl Iterator for OpusSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.at >= self.buf.len() {
            if self.total > 0 && self.pos >= self.total {
                return None;
            }
            if self.wait > 0 {
                self.wait -= 1;
                return Some(0.0);
            }
            match self.refill() {
                Fill::Data => {}
                Fill::End => return None,
                Fill::Wait => {
                    self.wait = WAIT * self.channels as usize;
                    return Some(0.0);
                }
            }
        }
        let s = self.buf[self.at];
        self.at += 1;
        if self.at % self.channels as usize == 0 {
            self.pos += 1;
        }
        Some(s as Sample)
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        NonZero::new(self.channels).unwrap_or(NonZero::<u16>::MIN)
    }

    fn sample_rate(&self) -> SampleRate {
        NonZero::new(RATE).expect("nonzero")
    }

    fn total_duration(&self) -> Option<Duration> {
        (self.total > 0).then(|| Duration::from_secs_f64(self.total as f64 / RATE as f64))
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        let err = |e: std::io::Error| SeekError::Other(Arc::new(e));
        let mut target = (pos.as_secs_f64() * RATE as f64) as u64;
        if self.total > 0 {
            target = target.min(self.total);
        }
        // Absolute sample (pre-skip included) to start decoding from.
        let goal = (target + self.pre_skip).saturating_sub(PREROLL);
        // Reopened by path: a finished download has been filed into the
        // library by now, and the link points there.
        let file = Growing::open(&self.path).map_err(err)?;
        self.live = file.live.clone();
        self.wait = 0;
        let mut reader = PacketReader::new(BufReader::new(file));
        let mut packets = 0;
        let mut at = 0u64;
        // Skip the two headers, then sum packet lengths up to `goal`.
        let mut queued = Vec::new();
        while let Ok(Some(p)) = reader.read_packet() {
            packets += 1;
            if packets <= 2 {
                continue;
            }
            let n = opus::packet::get_nb_samples(&p.data, RATE).unwrap_or(960) as u64;
            if at + n > goal {
                queued.push(p);
                break;
            }
            at += n;
        }
        self.reader = reader;
        let _ = self.decoder.reset_state();
        self.buf.clear();
        self.at = 0;
        // Output from `at` onward; drop everything before the target.
        self.discard = (target + self.pre_skip).saturating_sub(at);
        self.pos = target;
        let mut out = vec![0f32; MAX_FRAME * self.channels as usize];
        for p in queued {
            if let Ok(n) = self.decoder.decode_float(&p.data, &mut out, false) {
                let skip = (self.discard as usize).min(n);
                self.discard -= skip as u64;
                let ch = self.channels as usize;
                self.buf.extend(out[skip * ch..n * ch].iter().map(|s| s * self.gain));
            }
        }
        Ok(())
    }
}

