//! Resume sync: the queue, the song and the position, shared through a
//! `music-tui serve` server so another device (music-android, music-kodi, or
//! another music-tui) picks up where this one stopped. Tracks travel in
//! music-android's JSON shape; the newest record wins.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::SyncConfig;
use crate::model::{Library, Track};
use crate::{podcast, spotify, stream};

/// A track as music-android and music-kodi store it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Wire {
    pub key: String,
    pub title: String,
    pub artist: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub album_artist: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub album: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<u32>,
    #[serde(skip_serializing_if = "is_zero")]
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub art: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub uri: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isrc: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub feed: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What was playing, where, and when that was so.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Record {
    /// Unix ms; the newer record wins.
    pub updated: i64,
    pub device: String,
    pub queue: Vec<Wire>,
    pub index: usize,
    pub position_ms: u64,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// music-android's key for a song known only by artist and title.
pub fn hashed_key(artist: &str, title: &str) -> String {
    let d = sha1_smol::Sha1::from(format!("{}\0{}", artist.to_lowercase(), title.to_lowercase())).digest().bytes();
    format!("h:{}", d[..8].iter().map(|b| format!("{b:02x}")).collect::<String>())
}

// --- tracks ----------------------------------------------------------------

pub fn to_wire(t: &Track) -> Wire {
    let mut w = Wire {
        title: t.title.clone(),
        artist: t.track_artist.clone(),
        album_artist: t.album_artist.clone(),
        album: t.album.clone(),
        year: t.year,
        track_no: t.track_no,
        disc_no: t.disc_no,
        duration_ms: t.length_secs * 1000,
        ..Default::default()
    };
    if stream::is_url(&t.path) {
        let url = t.path.to_string_lossy().into_owned();
        w.art = stream::art(&t.path).unwrap_or_default();
        if t.length_secs == 0 {
            w.key = format!("radio:{}", crate::radio::station_id(&t.path).unwrap_or_else(|| short_hash(&url)));
        } else {
            w.key = format!("ep:{url}");
            if let Some(show) = podcast::show_of(&t.path) {
                w.feed = show.feed;
            }
            // The TUI keeps the date in the album and year fields.
            w.album.clear();
            w.year = None;
        }
        w.uri = url;
        return w;
    }
    if spotify::is_remote(&t.path) {
        let id = t.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        w.key = if let Some(v) = id.strip_prefix("yt-") {
            format!("yt:{v}")
        } else if let Some(d) = id.strip_prefix("dz").filter(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit())) {
            format!("dz:{d}")
        } else if id.len() == 17 && id.starts_with('h') && id[1..].bytes().all(|b| b.is_ascii_hexdigit()) {
            hashed_key(&t.track_artist, &t.title)
        } else {
            format!("sp:{id}")
        };
        w.isrc = spotify::isrc_for(&t.path).unwrap_or_default();
        return w;
    }
    w.key = hashed_key(&t.track_artist, &t.title);
    w
}

fn short_hash(s: &str) -> String {
    let d = sha1_smol::Sha1::from(s).digest().bytes();
    d[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The TUI track for a synced one: a local file when the library has the
/// song, otherwise the streamed track the TUI would make for it.
pub fn from_wire(w: &Wire, library: &Library) -> Option<Track> {
    let mut t = Track {
        id: 0,
        path: PathBuf::new(),
        title: w.title.clone(),
        track_artist: w.artist.clone(),
        album_artist: w.album_artist.clone(),
        album: w.album.clone(),
        year: w.year,
        track_no: w.track_no,
        disc_no: w.disc_no,
        length_secs: w.duration_ms / 1000,
    };
    let (kind, id) = w.key.split_once(':')?;
    let cover = (!w.art.is_empty() && w.art.starts_with("http")).then(|| w.art.clone());
    let isrc = (!w.isrc.is_empty()).then(|| w.isrc.clone());
    match kind {
        "radio" | "ep" => {
            if !w.uri.starts_with("http") {
                return None;
            }
            t.path = PathBuf::from(&w.uri);
            if let Some(a) = &cover {
                stream::set_art(&t.path, a);
            }
            if kind == "radio" {
                crate::radio::set_station_id(&t.path, id);
                t.length_secs = 0;
            } else {
                t.length_secs = t.length_secs.max(1);
            }
            return Some(t);
        }
        _ => {}
    }
    if let Some(local) = local_match(library, &w.artist, &w.title) {
        return Some(local.clone());
    }
    let stem = match kind {
        "sp" => id.to_string(),
        "dz" => format!("dz{id}"),
        "yt" => format!("yt-{id}"),
        _ => {
            let h = hashed_key(&w.artist, &w.title);
            format!("h{}", &h[2..])
        }
    };
    if kind != "yt" {
        spotify::register_extra(&stem, cover, isrc, w.year.map(|y| y.to_string()));
    }
    t.path = spotify::audio_dir().join(format!("{stem}.m4a"));
    Some(t)
}

fn local_match<'a>(library: &'a Library, artist: &str, title: &str) -> Option<&'a Track> {
    let main = |a: &str| a.split(',').next().unwrap_or("").trim().to_lowercase();
    let (artist, title) = (main(artist), title.to_lowercase());
    if title.is_empty() {
        return None;
    }
    library
        .tracks
        .iter()
        .find(|t| t.title.to_lowercase() == title && main(&t.track_artist) == artist)
}

pub fn record(queue: &[Track], index: usize, position_secs: f64, device: &str) -> Record {
    Record {
        updated: now_ms(),
        device: device.to_string(),
        queue: queue.iter().map(to_wire).collect(),
        index,
        position_ms: (position_secs * 1000.0).max(0.0) as u64,
    }
}

// --- client ----------------------------------------------------------------

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn url(cfg: &SyncConfig) -> String {
    format!("{}/v1/now", cfg.server.trim_end_matches('/'))
}

/// The server's record, if there is one.
pub fn pull(cfg: &SyncConfig) -> Result<Option<Record>> {
    let mut resp = agent(Duration::from_secs(8))
        .get(&url(cfg))
        .header("Authorization", &format!("Bearer {}", cfg.key))
        .call()?;
    match resp.status().as_u16() {
        200 => Ok(Some(serde_json::from_slice(&resp.body_mut().with_config().limit(64 << 20).read_to_vec()?)?)),
        204 => Ok(None),
        401 => bail!("the server doesn't know this key"),
        s => bail!("server answered {s}"),
    }
}

pub fn push(cfg: &SyncConfig, rec: &Record, timeout: Duration) -> Result<()> {
    let body = serde_json::to_vec(rec)?;
    let resp = agent(timeout)
        .put(&url(cfg))
        .header("Authorization", &format!("Bearer {}", cfg.key))
        .header("Content-Type", "application/json")
        .send(&body[..])?;
    match resp.status().as_u16() {
        200 => Ok(()),
        401 => bail!("the server doesn't know this key"),
        s => bail!("server answered {s}"),
    }
}

/// This machine's name, for telling devices apart.
pub fn device_name(cfg: &SyncConfig) -> String {
    if !cfg.device.is_empty() {
        return cfg.device.clone();
    }
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "computer".into());
    format!("music-tui on {host}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_key_matches_music_android() {
        // The first 8 bytes of SHA-1("queen\0bohemian rhapsody"), as Android's MessageDigest gives them.
        assert_eq!(hashed_key("Queen", "Bohemian Rhapsody"), "h:0c35682c37e07da5");
    }

    #[test]
    fn streamed_tracks_round_trip() {
        let lib = Library::default();
        for key in ["sp:0Y2SrByf4G3kbq2nBEHQRn", "dz:3135556", "yt:dQw4w9WgXcQ"] {
            let w = Wire { key: key.into(), title: "T".into(), artist: "A".into(), duration_ms: 200_000, ..Default::default() };
            let t = from_wire(&w, &lib).unwrap();
            assert_eq!(to_wire(&t).key, key);
            assert_eq!(t.length_secs, 200);
        }
    }

    #[test]
    fn stations_and_episodes_keep_their_urls() {
        let lib = Library::default();
        let ep = Wire { key: "ep:https://x/e.mp3".into(), uri: "https://x/e.mp3".into(), title: "E".into(), duration_ms: 60_000, ..Default::default() };
        let t = from_wire(&ep, &lib).unwrap();
        assert_eq!(to_wire(&t).key, "ep:https://x/e.mp3");
        let st = Wire { key: "radio:abc".into(), uri: "http://s/live".into(), title: "S".into(), ..Default::default() };
        let t = from_wire(&st, &lib).unwrap();
        assert_eq!(t.length_secs, 0);
        assert!(to_wire(&t).key.starts_with("radio:"));
    }

    #[test]
    fn local_songs_are_found_in_the_library() {
        let mine = Track {
            id: 1,
            path: "/m/a.flac".into(),
            title: "Babe".into(),
            track_artist: "Styx".into(),
            album_artist: "Styx".into(),
            album: "Cornerstone".into(),
            year: None,
            track_no: None,
            disc_no: None,
            length_secs: 266,
        };
        let lib = Library::new(vec![mine]);
        let w = Wire { key: hashed_key("Styx", "Babe"), title: "babe".into(), artist: "Styx".into(), ..Default::default() };
        assert_eq!(from_wire(&w, &lib).unwrap().path, PathBuf::from("/m/a.flac"));
        assert_eq!(to_wire(&lib.tracks[0]).key, hashed_key("Styx", "Babe"));
    }
}
