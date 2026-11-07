//! Internet radio, from Radio Browser (radio-browser.info): a free,
//! community-run directory of about 50,000 stations, no account needed.
//! A station is a `Track` whose path is its stream URL and whose length is
//! 0 (live). Favorites are kept in `radio.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::deezer::{Item, View};
use crate::model::Track;

/// Radio Browser's mirrors, tried in order.
const SERVERS: [&str; 3] = [
    "https://de1.api.radio-browser.info",
    "https://fi1.api.radio-browser.info",
    "https://de2.api.radio-browser.info",
];

/// Genres on the home page: (title, Radio Browser tag).
const GENRES: [(&str, &str); 9] = [
    ("Jazz", "jazz"),
    ("Classical", "classical"),
    ("Rock", "rock"),
    ("Electronic", "electronic"),
    ("Hip hop", "hip hop"),
    ("Lo-fi", "lofi"),
    ("Ambient", "ambient"),
    ("News", "news"),
    ("Talk", "talk"),
];

fn get(path: &str) -> Result<Value> {
    let mut last = None;
    for server in SERVERS {
        // Radio Browser asks clients to name themselves.
        match ureq::get(&format!("{server}/json/{path}"))
            .header("User-Agent", concat!("music-tui/", env!("CARGO_PKG_VERSION")))
            .call()
        {
            Ok(mut resp) => return Ok(serde_json::from_slice(&resp.body_mut().read_to_vec()?)?),
            Err(e) => last = Some(e),
        }
    }
    bail!("Radio Browser: {}", last.map(|e| e.to_string()).unwrap_or_default())
}

/// Station IDs by stream URL, for counting plays.
fn ids() -> &'static Mutex<HashMap<PathBuf, String>> {
    static IDS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    IDS.get_or_init(Default::default)
}

pub fn station_id(path: &std::path::Path) -> Option<String> {
    ids().lock().ok()?.get(path).cloned()
}

pub fn set_station_id(path: &std::path::Path, id: &str) {
    if let Ok(mut ids) = ids().lock() {
        ids.entry(path.to_path_buf()).or_insert_with(|| id.to_string());
    }
}

fn station(v: &Value) -> Option<Track> {
    let url = v["url_resolved"].as_str().filter(|u| !u.is_empty()).or(v["url"].as_str())?;
    if !url.starts_with("http") {
        return None;
    }
    let name = v["name"].as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    let path = PathBuf::from(url);
    let tags: Vec<String> = v["tags"]
        .as_str()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .take(3)
        .map(str::to_string)
        .collect();
    let mut about = Vec::new();
    if let Some(c) = v["countrycode"].as_str().filter(|c| !c.is_empty()) {
        about.push(c.to_string());
    }
    let codec = v["codec"].as_str().unwrap_or("");
    match v["bitrate"].as_u64().filter(|b| *b > 0) {
        Some(b) => about.push(format!("{codec} {b}k").trim().to_string()),
        None if !codec.is_empty() => about.push(codec.to_string()),
        None => {}
    }
    crate::stream::set_art(&path, v["favicon"].as_str().unwrap_or(""));
    if let (Some(id), Ok(mut ids)) = (v["stationuuid"].as_str(), ids().lock()) {
        ids.insert(path.clone(), id.to_string());
    }
    Some(Track {
        id: 0,
        path,
        title: name.to_string(),
        track_artist: if tags.is_empty() { "Radio".into() } else { tags.join(", ") },
        album_artist: String::new(),
        album: about.join(" · "),
        year: None,
        track_no: None,
        disc_no: None,
        length_secs: 0,
    })
}

fn stations(path: &str) -> Vec<Item> {
    let Ok(v) = get(path) else { return Vec::new() };
    let mut seen = std::collections::HashSet::new();
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(station)
        // Mirrors of one station often share a name or a stream.
        .filter(|t| seen.insert(t.title.to_lowercase()) & seen.insert(t.path.to_string_lossy().into_owned()))
        .map(Item::Track)
        .collect()
}

/// A URL path segment (spaces as %20, not +).
fn segment(s: &str) -> String {
    crate::spotify::url_encode(s).replace('+', "%20")
}

const LIST: &str = "hidebroken=true&order=clickcount&reverse=true&limit=60";

/// Stations by name, and by tag when the query is a genre.
pub fn search(q: &str) -> Vec<View> {
    let enc = crate::spotify::url_encode(q);
    let mut views = Vec::new();
    let named = stations(&format!("stations/search?name={enc}&{LIST}"));
    if !named.is_empty() {
        views.push(View { title: format!("Stations: {q}"), items: named });
    }
    let tagged = stations(&format!("stations/bytagexact/{}?{LIST}", segment(&q.to_lowercase())));
    if !tagged.is_empty() {
        views.push(View { title: format!("Genre: {q}"), items: tagged });
    }
    views
}

/// The two-letter country from the locale (`en_US.UTF-8` → `US`).
fn country() -> Option<String> {
    let lang = std::env::var("LC_ALL").ok().filter(|l| !l.is_empty()).or_else(|| std::env::var("LANG").ok())?;
    let cc = lang.split('.').next()?.split('_').nth(1)?;
    (cc.len() == 2).then(|| cc.to_uppercase())
}

/// The home sections: popular, near you, then genres, fetched in parallel.
pub fn home() -> Vec<View> {
    let mut wanted: Vec<(String, String)> = vec![("Popular".into(), "stations/topclick/60?hidebroken=true".into())];
    if let Some(cc) = country() {
        wanted.push((format!("Popular in {cc}"), format!("stations/bycountrycodeexact/{cc}?{LIST}")));
    }
    wanted.push(("Most voted".into(), "stations/topvote/60?hidebroken=true".into()));
    for (title, tag) in GENRES {
        wanted.push((title.into(), format!("stations/bytagexact/{}?{LIST}", segment(tag))));
    }
    let mut views: Vec<View> = std::thread::scope(|s| {
        let jobs: Vec<_> = wanted.iter().map(|(_, path)| s.spawn(move || stations(path))).collect();
        wanted
            .iter()
            .zip(jobs)
            .map(|((title, _), job)| View { title: title.clone(), items: job.join().unwrap_or_default() })
            .collect()
    });
    views.retain(|v| !v.items.is_empty());
    views
}

/// Tell Radio Browser a station was played (it ranks stations by plays,
/// and asks clients to report them).
pub fn count_play(path: &Path) {
    let Some(id) = ids().lock().ok().and_then(|m| m.get(path).cloned()) else { return };
    std::thread::spawn(move || {
        let _ = get(&format!("url/{id}"));
    });
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    favorites: Vec<Favorite>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Favorite {
    pub track: Track,
    #[serde(default)]
    pub art: String,
    #[serde(default)]
    pub id: String,
}

pub fn load_favorites() -> Vec<Track> {
    let store: Store = std::fs::read_to_string(config::radio_store_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    store
        .favorites
        .into_iter()
        .map(|f| {
            crate::stream::set_art(&f.track.path, &f.art);
            if !f.id.is_empty()
                && let Ok(mut ids) = ids().lock()
            {
                ids.insert(f.track.path.clone(), f.id);
            }
            f.track
        })
        .collect()
}

pub fn save_favorites(tracks: &[Track]) {
    let ids = ids().lock().map(|m| m.clone()).unwrap_or_default();
    let store = Store {
        favorites: tracks
            .iter()
            .map(|t| Favorite {
                track: t.clone(),
                art: crate::stream::art(&t.path).unwrap_or_default(),
                id: ids.get(&t.path).cloned().unwrap_or_default(),
            })
            .collect(),
    };
    let path = config::radio_store_path();
    let _ = config::ensure_parent(&path);
    if let Ok(body) = toml::to_string(&store) {
        let _ = std::fs::write(path, body);
    }
}
