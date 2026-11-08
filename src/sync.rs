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

/// One change to the library or podcasts. `at` is when it happened (Unix
/// ms); 1 marks a device's first upload, which any real change outranks.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Op {
    List { id: String, name: String, source: String, pos: i64, deleted: bool, at: i64 },
    Song { list: String, key: String, track: serde_json::Value, pos: i64, present: bool, at: i64 },
    Ep { key: String, pos: i64, played: bool, at: i64 },
    Sub { feed: String, show: serde_json::Value, present: bool, at: i64 },
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
        } else if id.len() == 17 && id.starts_with('h') && hashed_key(&t.track_artist, &t.title)[2..] == id[1..] {
            // A song another device knew only by artist and title. (music-tui's
            // own hashed imports keep their id, as the other apps were given it.)
            format!("h:{}", &id[1..])
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
    from_wire_in(w, Some(library))
}

/// The streamed track for a synced one, never a local file (for the
/// Streaming library, whose songs are all streamed).
pub fn from_wire_streamed(w: &Wire) -> Option<Track> {
    from_wire_in(w, None)
}

fn from_wire_in(w: &Wire, library: Option<&Library>) -> Option<Track> {
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
    if let Some(local) = library.and_then(|l| local_match(l, &w.artist, &w.title)) {
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
    from_start(Record {
        updated: now_ms(),
        device: device.to_string(),
        queue: queue.iter().map(to_wire).collect(),
        index,
        position_ms: (position_secs * 1000.0).max(0.0) as u64,
    })
}

/// Only podcast episodes carry their position; songs start over.
fn from_start(mut rec: Record) -> Record {
    if !rec.queue.get(rec.index).is_some_and(|w| w.key.starts_with("ep:")) {
        rec.position_ms = 0;
    }
    rec
}

// --- library and podcasts ------------------------------------------------------

/// A device's library and podcasts in the shape the server answers with.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub lists: Vec<List>,
    pub eps: std::collections::BTreeMap<String, Ep>,
    pub subs: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct List {
    pub id: String,
    pub name: String,
    pub source: String,
    pub tracks: Vec<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ep {
    pub pos: i64,
    pub played: bool,
}

/// What the server last confirmed: lists with their songs' keys, episodes,
/// subscribed feeds, and each part's version.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Base {
    pub versions: std::collections::HashMap<String, i64>,
    pub lists: Vec<(String, String, Vec<String>)>,
    pub eps: std::collections::BTreeMap<String, Ep>,
    pub subs: Vec<String>,
    /// Playlist ids by name, for lists another device made (music-tui
    /// knows its lists by name).
    pub ids: std::collections::HashMap<String, String>,
}

fn key_of(t: &serde_json::Value) -> String {
    t["key"].as_str().unwrap_or("").to_string()
}

fn feed_of(s: &serde_json::Value) -> String {
    s["feed"].as_str().unwrap_or("").to_string()
}

impl Base {
    /// The parts of `state` the server sent, recorded as confirmed.
    pub fn take(&mut self, state: &State, sent: &Answer) {
        if sent.lists.is_some() {
            self.ids = state.lists.iter().map(|l| (l.name.clone(), l.id.clone())).collect();
            self.lists = state.lists.iter().map(|l| (l.id.clone(), l.name.clone(), l.tracks.iter().map(key_of).collect())).collect();
        }
        if sent.eps.is_some() {
            self.eps = state.eps.clone();
            self.subs = state.subs.iter().map(feed_of).collect();
        }
        self.versions = sent.versions.clone();
    }
}

/// The changes from `base` to `cur`. Without a base (the first sync) every
/// item goes up as of time 1, so changes other devices already made win.
pub fn diff(base: Option<&Base>, cur: &State, now: i64) -> Vec<Op> {
    let at = if base.is_some() { now } else { 1 };
    let empty = Base::default();
    let b = base.unwrap_or(&empty);
    let mut ops = Vec::new();
    let old: std::collections::HashMap<&str, (&str, std::collections::HashSet<&str>)> =
        b.lists.iter().map(|(id, name, keys)| (id.as_str(), (name.as_str(), keys.iter().map(String::as_str).collect()))).collect();
    for (i, l) in cur.lists.iter().enumerate() {
        let was = old.get(l.id.as_str());
        if was.is_none_or(|(name, _)| *name != l.name) {
            ops.push(Op::List { id: l.id.clone(), name: l.name.clone(), source: l.source.clone(), pos: i as i64, deleted: false, at });
        }
        let had = was.map(|(_, k)| k.clone()).unwrap_or_default();
        let mut now_keys = std::collections::HashSet::new();
        for (j, t) in l.tracks.iter().enumerate() {
            let k = key_of(t);
            if k.is_empty() || !now_keys.insert(k.clone()) {
                continue;
            }
            if !had.contains(k.as_str()) {
                ops.push(Op::Song { list: l.id.clone(), key: k, track: t.clone(), pos: j as i64, present: true, at });
            }
        }
        for k in had {
            if !now_keys.contains(k) {
                ops.push(Op::Song { list: l.id.clone(), key: k.to_string(), track: serde_json::json!({"key": k}), pos: 0, present: false, at });
            }
        }
    }
    let ids: std::collections::HashSet<&str> = cur.lists.iter().map(|l| l.id.as_str()).collect();
    for (id, name, _) in &b.lists {
        if !ids.contains(id.as_str()) {
            ops.push(Op::List { id: id.clone(), name: name.clone(), source: String::new(), pos: 0, deleted: true, at });
        }
    }
    for (k, e) in &cur.eps {
        if b.eps.get(k) != Some(e) {
            ops.push(Op::Ep { key: k.clone(), pos: e.pos, played: e.played, at });
        }
    }
    for k in b.eps.keys() {
        if !cur.eps.contains_key(k) {
            ops.push(Op::Ep { key: k.clone(), pos: 0, played: false, at });
        }
    }
    let feeds: std::collections::HashSet<String> = cur.subs.iter().map(feed_of).collect();
    for s in &cur.subs {
        let f = feed_of(s);
        if !f.is_empty() && !b.subs.contains(&f) {
            ops.push(Op::Sub { feed: f, show: s.clone(), present: true, at });
        }
    }
    for f in &b.subs {
        if !feeds.contains(f) {
            ops.push(Op::Sub { feed: f.clone(), show: serde_json::json!({"feed": f}), present: false, at });
        }
    }
    ops
}

/// The server's answer: whichever parts changed, and the versions.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Answer {
    pub versions: std::collections::HashMap<String, i64>,
    pub lists: Option<Vec<List>>,
    pub eps: Option<std::collections::BTreeMap<String, Ep>>,
    pub subs: Option<Vec<serde_json::Value>>,
}

pub fn sync_state(cfg: &SyncConfig, versions: &std::collections::HashMap<String, i64>, ops: &[Op], timeout: Duration) -> Result<Answer> {
    let body = serde_json::to_vec(&serde_json::json!({"versions": versions, "ops": ops}))?;
    let mut resp = agent(timeout)
        .post(&format!("{}/v1/sync", cfg.server.trim_end_matches('/')))
        .header("Authorization", &format!("Bearer {}", cfg.key))
        .header("Content-Type", "application/json")
        .send(&body[..])?;
    match resp.status().as_u16() {
        200 => Ok(serde_json::from_slice(&resp.body_mut().with_config().limit(64 << 20).read_to_vec()?)?),
        401 => bail!("the server doesn't know this key"),
        s => bail!("server answered {s}"),
    }
}

/// music-android's id for a playlist by name (Java's String.hashCode in hex).
pub fn list_id(name: &str) -> String {
    if name == spotify::LIKED {
        return "liked".into();
    }
    let mut h: i32 = 0;
    for u in name.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(u as i32);
    }
    format!("s{:x}", h as u32)
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
        200 => Ok(Some(from_start(serde_json::from_slice(&resp.body_mut().with_config().limit(64 << 20).read_to_vec()?)?))),
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
    fn only_episodes_keep_their_position() {
        let rec = |key: &str| Record {
            updated: 1,
            device: "d".into(),
            queue: vec![Wire { key: key.into(), ..Default::default() }],
            index: 0,
            position_ms: 90_000,
        };
        assert_eq!(from_start(rec("ep:https://example.com/1.mp3")).position_ms, 90_000);
        assert_eq!(from_start(rec("sp:0Y2SrByf4G3kbq2nBEHQRn")).position_ms, 0);
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
