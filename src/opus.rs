//! Ogg Opus playback (YouTube's best audio stream), which rodio's symphonia
//! backend can't decode. Decodes through libopus at 48 kHz. Seeking rescans
//! packet lengths from the start (cheap: no decoding) and decodes the last
//! 80 ms before the target to let the decoder converge, as the Opus spec
//! recommends.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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

pub struct OpusSource {
    path: std::path::PathBuf,
    reader: PacketReader<BufReader<File>>,
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
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let total_granule = last_granule(&file).unwrap_or(0);
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

    /// Decode the next packet into `buf`. False at the end of the stream.
    fn refill(&mut self) -> bool {
        let mut out = vec![0f32; MAX_FRAME * self.channels as usize];
        loop {
            let Ok(Some(packet)) = self.reader.read_packet() else { return false };
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
            return true;
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
            if !self.refill() {
                return None;
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
        let file = File::open(&self.path).map_err(err)?;
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
