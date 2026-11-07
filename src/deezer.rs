//! Browser tab backend: Deezer's public API (no key, no account) for music
//! search, charts, albums/artists/playlists and recommendations, plus an
//! anonymous YouTube Music song search as a fallback. Tracks found here are
//! ordinary Streaming tracks (`<cache>/spotify/audio/dz<id>.m4a`): they're
//! fetched from YouTube on play exactly like imported ones.

use std::collections::HashSet;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::model::Track;
use crate::spotify;

/// One row in the Browser tab.
#[derive(Clone, Debug)]
pub enum Item {
    Track(Track),
    Album { id: u64, title: String, artist: String, cover: String },
    Artist { id: u64, name: String, picture: String },
    Playlist { id: u64, title: String, count: u64, cover: String },
}

/// A titled list of items: a left-pane section, or an opened album/artist/
/// playlist.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub title: String,
    pub items: Vec<Item>,
}

impl View {
    /// The playable tracks in this view, in order.
    pub fn tracks(&self) -> Vec<Track> {
        self.items
            .iter()
            .filter_map(|i| match i {
                Item::Track(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }
}

/// What to open when an album/artist/playlist row is activated.
#[derive(Clone, Debug)]
pub enum Open {
    Album(u64, String),
    Artist(u64, String),
    Playlist(u64, String),
}

fn get(path: &str) -> Result<Value> {
    let body = spotify::http_get(&format!("https://api.deezer.com/{path}"))?;
    let v: Value = serde_json::from_slice(&body)?;
    if let Some(e) = v.get("error") {
        bail!("Deezer: {}", e["message"].as_str().unwrap_or("request failed"));
    }
    Ok(v)
}

fn data(v: &Value) -> &[Value] {
    v["data"].as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// A Deezer track JSON object as a Streaming track. `album` supplies the
/// album for listings that omit it (an album's own track list).
fn track(v: &Value, album: Option<&Value>) -> Option<Track> {
    let id = v["id"].as_u64()?;
    let album = album.unwrap_or(&v["album"]);
    let key = format!("dz{id}");
    let cover = s(&album["cover_xl"]);
    spotify::register_extra(
        &key,
        (!cover.is_empty()).then_some(cover),
        v["isrc"].as_str().map(String::from),
        album["release_date"].as_str().map(String::from),
    );
    let artist = s(&v["artist"]["name"]);
    Some(Track {
        id: 0,
        path: spotify::audio_dir().join(format!("{key}.m4a")),
        title: s(&v["title"]),
        track_artist: artist.clone(),
        album_artist: album["artist"]["name"].as_str().map(String::from).unwrap_or(artist),
        album: s(&album["title"]),
        year: album["release_date"].as_str().and_then(|d| d.get(0..4)?.parse().ok()),
        track_no: v["track_position"].as_u64().map(|n| n as u32),
        disc_no: v["disk_number"].as_u64().map(|n| n as u32),
        length_secs: v["duration"].as_u64().unwrap_or(0),
    })
}

fn tracks(v: &Value, album: Option<&Value>) -> Vec<Item> {
    data(v).iter().filter_map(|t| track(t, album)).map(Item::Track).collect()
}

fn albums(v: &Value) -> Vec<Item> {
    data(v)
        .iter()
        .filter_map(|a| {
            Some(Item::Album {
                id: a["id"].as_u64()?,
                title: s(&a["title"]),
                artist: s(&a["artist"]["name"]),
                cover: s(&a["cover_medium"]),
            })
        })
        .collect()
}

fn artists(v: &Value) -> Vec<Item> {
    data(v)
        .iter()
        .filter_map(|a| {
            Some(Item::Artist {
                id: a["id"].as_u64()?,
                name: s(&a["name"]),
                picture: s(&a["picture_medium"]),
            })
        })
        .collect()
}

fn playlists(v: &Value) -> Vec<Item> {
    data(v)
        .iter()
        .filter_map(|p| {
            Some(Item::Playlist {
                id: p["id"].as_u64()?,
                title: s(&p["title"]),
                count: p["nb_tracks"].as_u64().unwrap_or(0),
                cover: s(&p["picture_medium"]),
            })
        })
        .collect()
}

fn enc(q: &str) -> String {
    q.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Search everything: one section per kind that has results. Songs fall back
/// to YouTube Music when Deezer has none.
pub fn search(q: &str) -> Result<Vec<View>> {
    let q = enc(q);
    let mut out = Vec::new();
    let mut push = |title: &str, items: Vec<Item>| {
        if !items.is_empty() {
            out.push(View { title: title.into(), items });
        }
    };
    let songs = tracks(&get(&format!("search?q={q}&limit=50"))?, None);
    let no_songs = songs.is_empty();
    push("Songs", songs);
    push("Artists", artists(&get(&format!("search/artist?q={q}&limit=15"))?));
    push("Albums", albums(&get(&format!("search/album?q={q}&limit=25"))?));
    push("Playlists", playlists(&get(&format!("search/playlist?q={q}&limit=25"))?));
    if no_songs {
        push("YouTube Music", ytm_search(&q));
    }
    Ok(out)
}

/// The landing sections: Deezer's top tracks, albums and playlists.
pub fn home() -> Result<Vec<View>> {
    Ok(vec![
        View { title: "Top songs".into(), items: tracks(&get("chart/0/tracks?limit=100")?, None) },
        View { title: "Top albums".into(), items: albums(&get("chart/0/albums?limit=50")?) },
        View { title: "Popular playlists".into(), items: playlists(&get("chart/0/playlists?limit=50")?) },
    ])
}

/// "New releases for you": albums, EPs and singles from the last `days`
/// days by the given artists, newest first.
pub fn new_releases(artists: &[String], days: i64) -> Result<View> {
    let since = date_days_ago(days);
    let mut found: Vec<(String, Item)> = Vec::new();
    for name in artists {
        let hit = get(&format!("search/artist?q={}&limit=1", enc(name)))?;
        let Some(artist) = data(&hit).first() else { continue };
        // Only trust an exact name match; a fuzzy hit is someone else.
        if !artist["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            continue;
        }
        let Some(id) = artist["id"].as_u64() else { continue };
        let list = get(&format!("artist/{id}/albums?limit=50"))?;
        for a in data(&list) {
            let date = s(&a["release_date"]);
            if date.as_str() < since.as_str() {
                continue;
            }
            let kind = a["record_type"].as_str().unwrap_or("album");
            let Some(album_id) = a["id"].as_u64() else { continue };
            found.push((
                date.clone(),
                Item::Album {
                    id: album_id,
                    title: s(&a["title"]),
                    artist: format!("{} · {kind} · {date}", artist["name"].as_str().unwrap_or(name)),
                    cover: s(&a["cover_medium"]),
                },
            ));
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let mut seen = HashSet::new();
    let items: Vec<Item> = found
        .into_iter()
        .filter(|(_, i)| matches!(i, Item::Album { id, .. } if seen.insert(*id)))
        .map(|(_, i)| i)
        .collect();
    if items.is_empty() {
        bail!("no new releases");
    }
    Ok(View { title: "New releases for you".into(), items })
}

/// `YYYY-MM-DD` for `days` days before today (UTC).
fn date_days_ago(days: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = now / 86_400 - days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The tracks behind an album/artist/playlist row.
pub fn open(what: &Open) -> Result<View> {
    match what {
        Open::Album(id, title) => {
            let album = get(&format!("album/{id}"))?;
            Ok(View { title: title.clone(), items: tracks(&album["tracks"], Some(&album)) })
        }
        Open::Artist(id, name) => {
            let mut items = tracks(&get(&format!("artist/{id}/top?limit=50"))?, None);
            items.extend(albums(&get(&format!("artist/{id}/albums?limit=50"))?));
            items.extend(artists(&get(&format!("artist/{id}/related?limit=20"))?));
            Ok(View { title: name.clone(), items })
        }
        Open::Playlist(id, title) => Ok(View {
            title: title.clone(),
            items: tracks(&get(&format!("playlist/{id}/tracks?limit=300"))?, None),
        }),
    }
}

/// A "For you" mix from the user's most-liked artists: each seed artist's
/// Deezer radio (similar music), minus anything already liked, interleaved
/// so no one seed dominates.
pub fn for_you(seed_artists: &[String], liked: &HashSet<(String, String)>) -> Result<View> {
    let mut radios: Vec<Vec<Track>> = Vec::new();
    for name in seed_artists {
        let found = get(&format!("search/artist?q={}&limit=1", enc(name)))?;
        let Some(id) = data(&found).first().and_then(|a| a["id"].as_u64()) else {
            continue;
        };
        let radio = get(&format!("artist/{id}/radio?limit=25"))?;
        radios.push(
            data(&radio)
                .iter()
                .filter_map(|t| track(t, None))
                .filter(|t| !liked.contains(&(t.track_artist.to_lowercase(), t.title.to_lowercase())))
                .collect(),
        );
    }
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for round in 0..25 {
        for r in &radios {
            if let Some(t) = r.get(round)
                && seen.insert(t.path.clone())
            {
                items.push(Item::Track(t.clone()));
            }
        }
    }
    if items.is_empty() {
        bail!("no recommendations found");
    }
    Ok(View { title: "For you".into(), items })
}

/// Library details for a song, from Deezer.
pub struct Details {
    pub album_artist: String,
    pub album: String,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
}

/// Details for a full Deezer track object: the album artist needs one more
/// request for the album (track objects only name the track's artist).
fn details(v: &Value) -> Option<Details> {
    let album = get(&format!("album/{}", v["album"]["id"].as_u64()?)).ok()?;
    let date = v["release_date"].as_str().or(album["release_date"].as_str()).unwrap_or("");
    Some(Details {
        album_artist: album["artist"]["name"].as_str()?.to_string(),
        album: s(&album["title"]),
        track_no: v["track_position"].as_u64().map(|n| n as u32),
        disc_no: v["disk_number"].as_u64().map(|n| n as u32),
        year: date.get(0..4).and_then(|y| y.parse().ok()),
    })
}

/// Look a song up by its Deezer track id.
pub fn by_id(id: &str) -> Option<Details> {
    details(&get(&format!("track/{id}")).ok()?)
}

/// Look a song up by ISRC (the export's recording code).
pub fn by_isrc(isrc: &str) -> Option<Details> {
    details(&get(&format!("track/isrc:{}", enc(isrc))).ok()?)
}

/// Look a song up by artist and title; only an exact (case-insensitive)
/// title match from a matching artist is trusted.
pub fn by_name(artist: &str, title: &str) -> Option<Details> {
    let main = artist.split(',').next().unwrap_or(artist).trim();
    let q = enc(&format!("artist:\"{main}\" track:\"{title}\""));
    let found = get(&format!("search?q={q}&limit=5")).ok()?;
    let hit = data(&found).iter().find(|t| {
        t["title"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(title))
            && t["artist"]["name"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(main))
    })?;
    details(&get(&format!("track/{}", hit["id"].as_u64()?)).ok()?)
}

/// A song on Deezer by artist and title, loosely: the first result whose
/// artist matches. Used to turn YouTube picks into proper tracks.
pub fn find(artist: &str, title: &str) -> Option<Track> {
    let want = artist.to_lowercase();
    let artist_ok = |t: &Value| {
        t["artist"]["name"].as_str().is_some_and(|a| {
            let a = a.to_lowercase();
            a.contains(&want) || want.contains(&a)
        })
    };
    for q in [format!("artist:\"{artist}\" track:\"{title}\""), format!("{artist} {title}")] {
        let found = get(&format!("search?q={}&limit=5", enc(&q))).ok()?;
        if let Some(hit) = data(&found).iter().find(|t| artist_ok(t)) {
            return track(hit, None);
        }
    }
    // The "artist" may just be the uploader's channel: trust an exact title.
    let norm = |x: &str| x.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let found = get(&format!("search?q={}&limit=5", enc(title))).ok()?;
    let hit = data(&found).iter().find(|t| t["title"].as_str().is_some_and(|x| norm(x) == norm(title)))?;
    track(hit, None)
}

/// Where "go to artist / album" leads for a track. Exact for Deezer tracks
/// and ones with an ISRC; otherwise found by name.
pub fn locate(t: &Track, album: bool) -> Result<Open> {
    let id = t.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let exact = match (id.strip_prefix("dz"), spotify::isrc_for(&t.path)) {
        (Some(dz), _) => get(&format!("track/{dz}")).ok(),
        (None, Some(isrc)) => get(&format!("track/isrc:{}", enc(&isrc))).ok(),
        _ => None,
    };
    if let Some(v) = exact.filter(|v| v["id"].as_u64().is_some()) {
        let open = if album {
            v["album"]["id"].as_u64().map(|i| Open::Album(i, s(&v["album"]["title"])))
        } else {
            v["artist"]["id"].as_u64().map(|i| Open::Artist(i, s(&v["artist"]["name"])))
        };
        if let Some(o) = open {
            return Ok(o);
        }
    }
    let main = t.track_artist.split(',').next().unwrap_or(&t.track_artist).trim();
    if album {
        if t.album.is_empty() {
            bail!("'{}' has no album", t.title);
        }
        let q = enc(&format!("artist:\"{main}\" album:\"{}\"", t.album));
        let found = get(&format!("search/album?q={q}&limit=1"))?;
        let a = data(&found).first().context("album not found on Deezer")?;
        Ok(Open::Album(a["id"].as_u64().context("album not found")?, s(&a["title"])))
    } else {
        let found = get(&format!("search/artist?q={}&limit=1", enc(main)))?;
        let a = data(&found).first().context("artist not found on Deezer")?;
        Ok(Open::Artist(a["id"].as_u64().context("artist not found")?, s(&a["name"])))
    }
}

/// A song radio: YouTube Music's mix for `seed`, each pick resolved on
/// Deezer for proper metadata and covers (kept as the plain YouTube upload
/// when Deezer doesn't have it). The pick's exact video is remembered so
/// it's the one downloaded.
pub fn radio(seed: &Track, count: usize) -> Result<View> {
    let Some(video) = spotify::video_for(seed) else { bail!("couldn't find '{}' on YouTube", seed.title) };
    let songs = spotify::youtube_radio(&video, count);
    if songs.is_empty() {
        bail!("YouTube has no radio for '{}'", seed.title);
    }
    // Lookups overlap their network waits; the per-host throttle in
    // `http_get` keeps the request rate within Deezer's limits.
    let resolved: Vec<Track> = std::thread::scope(|scope| {
        let jobs: Vec<_> = songs
            .iter()
            .map(|song| {
                scope.spawn(move || {
                    let t = find(&song.artist, &song.title).unwrap_or_else(|| Track {
                        id: 0,
                        path: spotify::audio_dir().join(format!("yt-{}.m4a", song.video)),
                        title: song.title.clone(),
                        track_artist: song.artist.clone(),
                        album_artist: song.artist.clone(),
                        album: String::new(),
                        year: None,
                        track_no: None,
                        disc_no: None,
                        length_secs: 0,
                    });
                    spotify::set_source(&t, &song.video);
                    t
                })
            })
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok()).collect()
    });
    let mut seen = HashSet::new();
    seen.insert((seed.track_artist.to_lowercase(), seed.title.to_lowercase()));
    let items = resolved
        .into_iter()
        .filter(|t| seen.insert((t.track_artist.to_lowercase(), t.title.to_lowercase())))
        .map(Item::Track)
        .collect();
    Ok(View { title: format!("Radio: {}", seed.title), items })
}

/// Album-cover URL for a Deezer track id (covers of tracks liked in an
/// earlier session, whose search-time details are gone).
pub fn track_cover(id: &str) -> Option<String> {
    let v = get(&format!("track/{id}")).ok()?;
    v["album"]["cover_xl"].as_str().map(String::from)
}

/// Anonymous YouTube Music song search. Results carry only a title.
fn ytm_search(q: &str) -> Vec<Item> {
    let Ok(out) = Command::new("yt-dlp")
        .args(["--flat-playlist", "--no-warnings", "--playlist-items", "1:15", "--print"])
        .arg("%(id)s\t%(title)s")
        .arg(format!("https://music.youtube.com/search?q={q}#songs"))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (id, title) = l.split_once('\t')?;
            (id.len() == 11).then(|| {
                Item::Track(Track {
                    id: 0,
                    path: spotify::audio_dir().join(format!("yt-{id}.m4a")),
                    title: title.to_string(),
                    track_artist: "YouTube Music".into(),
                    album_artist: "YouTube Music".into(),
                    album: String::new(),
                    year: None,
                    track_no: None,
                    disc_no: None,
                    length_secs: 0,
                })
            })
        })
        .collect()
}
