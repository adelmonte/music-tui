//! Spotify tab backend: imports a CSV export of the user's Spotify library
//! (TuneMyMusic or Exportify format — no account login), fetches each track's
//! audio from YouTube via `yt-dlp` on demand, and looks up album covers through
//! Spotify's public oEmbed endpoint. Fetched files are tagged (title, artist,
//! album, date, ISRC, embedded cover) so they can be added to the local
//! library as-is.
//!
//! An imported track is an ordinary `Track` whose `path` is a stable per-id
//! name, `<cache>/spotify/audio/<spotify id>.m4a`. Until that exists the
//! track "needs fetching". Fetched audio itself is filed like a music library
//! (`<cache>/spotify/music/Album Artist/[Year] Album/NN Title.m4a`) and the
//! per-id name is a symlink to it, so it plays like any local file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use image::DynamicImage;

use crate::config;
use crate::model::Track;
use crate::playlist::Playlist;
use crate::scan;

/// Name given to the user's saved/liked tracks, always listed first.
pub const LIKED: &str = "Liked Songs";

pub fn audio_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| config::spotify_cache_dir().join("audio"))
}

fn covers_dir() -> PathBuf {
    config::spotify_cache_dir().join("covers")
}

/// Where fetched songs are filed, organized like a music library.
pub fn music_root() -> PathBuf {
    config::spotify_cache_dir().join("music")
}

/// Export fields that have no home on `Track` but are worth keeping for
/// tagging and cover lookup.
#[derive(Clone, Default)]
struct Extra {
    /// Album-image URL (Exportify includes one), so covers skip oEmbed.
    cover_url: Option<String>,
    isrc: Option<String>,
    /// Full release date ("1969-01-01") when the export has one.
    release: Option<String>,
}

/// `Extra`s from the last import, keyed by track id.
fn extras() -> &'static Mutex<HashMap<String, Extra>> {
    static EXTRAS: OnceLock<Mutex<HashMap<String, Extra>>> = OnceLock::new();
    EXTRAS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record export-style extras for a track found elsewhere (e.g. a Deezer
/// result), so its cover and tags resolve the same way imported ones do.
pub fn register_extra(id: &str, cover_url: Option<String>, isrc: Option<String>, release: Option<String>) {
    if let Ok(mut e) = extras().lock() {
        e.insert(id.to_string(), Extra { cover_url, isrc, release });
    }
}

/// The ISRC (recording code) known for a track, if any.
pub fn isrc_for(path: &Path) -> Option<String> {
    extra_for(&track_id(path)?).isrc
}

fn extra_for(id: &str) -> Extra {
    extras().lock().ok().and_then(|e| e.get(id).cloned()).unwrap_or_default()
}

/// True if this track came from the Spotify export.
pub fn is_remote(path: &Path) -> bool {
    path.starts_with(audio_dir())
}

/// True for a track that is one specific YouTube video (a YouTube Music
/// search result) rather than a song to find a match for.
pub fn track_is_direct(path: &Path) -> bool {
    track_id(path).is_some_and(|id| id.starts_with("yt-"))
}

/// True if this is a Spotify track whose audio hasn't been fetched yet.
pub fn needs_fetch(path: &Path) -> bool {
    is_remote(path) && !path.exists()
}

fn track_id(path: &Path) -> Option<String> {
    path.file_stem().map(|s| s.to_string_lossy().into_owned())
}

// --- CSV import ------------------------------------------------------------

/// Column indices resolved from a CSV header row. Header names differ between
/// exporters, so each field accepts every known spelling.
struct Columns {
    title: usize,
    artist: usize,
    album: Option<usize>,
    playlist: Option<usize>,
    kind: Option<usize>,
    id: Option<usize>,
    isrc: Option<usize>,
    duration_ms: Option<usize>,
    cover: Option<usize>,
    release: Option<usize>,
    track_no: Option<usize>,
    disc_no: Option<usize>,
    added_at: Option<usize>,
}

impl Columns {
    fn resolve(headers: &csv::StringRecord) -> Option<Columns> {
        let names: Vec<String> = headers.iter().map(|h| h.trim().to_lowercase()).collect();
        let find = |cands: &[&str]| names.iter().position(|n| cands.contains(&n.as_str()));
        Some(Columns {
            title: find(&["track name", "title", "name"])?,
            artist: find(&["artist name", "artist name(s)", "artist", "artists"])?,
            album: find(&["album", "album name"]),
            playlist: find(&["playlist name", "playlist"]),
            kind: find(&["type"]),
            id: find(&["spotify - id", "track uri", "spotify uri", "track id", "spotify id"]),
            isrc: find(&["isrc"]),
            duration_ms: find(&["track duration (ms)", "duration (ms)", "duration_ms"]),
            cover: find(&["album image url"]),
            release: find(&["album release date", "release date"]),
            track_no: find(&["track number"]),
            disc_no: find(&["disc number"]),
            added_at: find(&["added at", "date added"]),
        })
    }
}

/// Read the export at `path` (a CSV file, or a directory of them) into
/// playlists, with "Liked Songs" first and the rest in order of appearance.
pub fn import(path: &Path) -> Result<Vec<Playlist>> {
    let files: Vec<PathBuf> = if path.is_dir() {
        let mut v: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("reading {}", path.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv")))
            .collect();
        v.sort();
        v
    } else {
        vec![path.to_path_buf()]
    };
    if files.is_empty() {
        bail!("no CSV files in {}", path.display());
    }

    let mut lists: Vec<Playlist> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut extra = HashMap::new();
    let mut skipped = Vec::new();
    for file in &files {
        // A folder export can hold non-track files (Exportify's saved
        // albums); skip those rather than failing the whole import.
        if let Err(e) = import_file(file, &mut lists, &mut index, &mut extra) {
            skipped.push(e);
        }
    }
    if lists.is_empty()
        && let Some(e) = skipped.into_iter().next()
    {
        return Err(e);
    }
    if let Ok(mut e) = extras().lock() {
        e.extend(extra);
    }
    // `Track::id` is otherwise unused for Spotify tracks; it records each
    // track's age rank in its list (0 = oldest) so sorting can restore it.
    // `import_file` left either the "Added At" time (as a sortable number)
    // or the row's position there.
    for list in &mut lists {
        list.tracks.sort_by_key(|t| t.id);
        for (i, t) in list.tracks.iter_mut().enumerate() {
            t.id = i as i64;
        }
    }
    if let Some(i) = lists.iter().position(|p| p.name == LIKED) {
        let liked = lists.remove(i);
        lists.insert(0, liked);
    }
    Ok(lists)
}

fn import_file(
    file: &Path,
    lists: &mut Vec<Playlist>,
    index: &mut HashMap<String, usize>,
    extras: &mut HashMap<String, Extra>,
) -> Result<()> {
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(file)
        .with_context(|| format!("opening {}", file.display()))?;
    let headers = rdr.headers()?.clone();
    let Some(cols) = Columns::resolve(&headers) else {
        bail!("{}: no track/artist name columns", file.display());
    };
    // Exportify writes one file per playlist, named after it.
    let file_list_name = file
        .file_stem()
        .map(|s| s.to_string_lossy().replace('_', " "))
        .unwrap_or_default();

    for rec in rdr.records() {
        let Ok(rec) = rec else { continue };
        let get = |i: Option<usize>| i.and_then(|i| rec.get(i)).map(str::trim).unwrap_or("");
        let title = get(Some(cols.title));
        let artist = get(Some(cols.artist));
        if title.is_empty() {
            continue;
        }
        let liked = get(cols.kind).eq_ignore_ascii_case("favorite")
            || file_list_name.eq_ignore_ascii_case("liked songs");
        let list_name = if liked {
            LIKED.to_string()
        } else {
            let n = get(cols.playlist);
            if n.is_empty() { file_list_name.clone() } else { n.to_string() }
        };

        let id = spotify_id(get(cols.id))
            .or_else(|| safe_id(get(cols.isrc)))
            .unwrap_or_else(|| hashed_id(artist, title));
        let path = audio_dir().join(format!("{id}.m4a"));
        let length_secs = get(cols.duration_ms)
            .parse::<u64>()
            .map(|ms| ms / 1000)
            .ok()
            .or_else(|| path.exists().then(|| scan::read_length(&path)).flatten())
            .unwrap_or(0);
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let release = get(cols.release);
        extras.insert(
            id.clone(),
            Extra {
                cover_url: opt(get(cols.cover)),
                isrc: opt(get(cols.isrc)),
                release: opt(release),
            },
        );

        let track = Track {
            id: 0,
            path,
            title: title.to_string(),
            track_artist: artist.to_string(),
            album_artist: artist.to_string(),
            album: get(cols.album).to_string(),
            year: release.get(0..4).and_then(|y| y.parse().ok()),
            track_no: get(cols.track_no).parse().ok(),
            disc_no: get(cols.disc_no).parse().ok(),
            length_secs,
        };
        let i = *index.entry(list_name.clone()).or_insert_with(|| {
            lists.push(Playlist { name: list_name, tracks: Vec::new() });
            lists.len() - 1
        });
        // "2024-11-19T12:34:56Z" -> 20241119123456; rows without a time keep
        // their position (a TuneMyMusic export lists oldest first).
        let added: String = get(cols.added_at).chars().filter(char::is_ascii_digit).collect();
        let mut track = track;
        track.id = added.get(..14).and_then(|d| d.parse().ok()).unwrap_or(lists[i].tracks.len() as i64);
        lists[i].tracks.push(track);
    }
    Ok(())
}

/// A bare Spotify track id from either a raw id or a `spotify:track:` URI.
fn spotify_id(s: &str) -> Option<String> {
    safe_id(s.rsplit(':').next().unwrap_or(s))
}

/// `s` if it's usable as a filename stem (alphanumeric), else None.
fn safe_id(s: &str) -> Option<String> {
    (!s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())).then(|| s.to_string())
}

fn hashed_id(artist: &str, title: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&(artist.to_lowercase(), title.to_lowercase()), &mut h);
    format!("h{:016x}", std::hash::Hasher::finish(&h))
}

// --- library store ---------------------------------------------------------

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    #[serde(default)]
    lists: Vec<Playlist>,
}

/// The Streaming library: the imported export plus everything liked or added
/// since. Empty if it doesn't exist yet.
pub fn load_store() -> Vec<Playlist> {
    std::fs::read_to_string(config::streaming_store_path())
        .ok()
        .and_then(|s| toml::from_str::<StoreFile>(&s).ok())
        .map(|f| f.lists)
        .unwrap_or_default()
}

pub fn save_store(lists: &[Playlist]) {
    let path = config::streaming_store_path();
    let _ = config::ensure_parent(&path);
    let file = StoreFile { lists: lists.to_vec() };
    if let Ok(body) = toml::to_string(&file) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Add an import's lists and tracks to the store without removing anything,
/// and re-order each list by the import (whose `id`s rank tracks oldest to
/// newest, by "Added At" when the export has it). Tracks only in the store —
/// added in music-tui, or gone from the export — stay, ranked as newest.
/// Returns how many tracks were added.
pub fn merge_import(store: &mut Vec<Playlist>, imported: Vec<Playlist>) -> usize {
    let mut added = 0;
    for list in imported {
        let i = match store.iter().position(|p| p.name == list.name) {
            Some(i) => i,
            None => {
                store.push(Playlist { name: list.name.clone(), tracks: Vec::new() });
                store.len() - 1
            }
        };
        let target = &mut store[i];
        let rank: HashMap<PathBuf, i64> = list.tracks.iter().map(|t| (t.path.clone(), t.id)).collect();
        let newest = list.tracks.iter().map(|t| t.id + 1).max().unwrap_or(0);
        for t in list.tracks {
            if !target.tracks.iter().any(|x| x.path == t.path) {
                target.tracks.push(t);
                added += 1;
            }
        }
        // Sort key: the import's rank, else after every imported track in
        // the store's existing order.
        let mut keyed: Vec<(i64, Track)> = target
            .tracks
            .drain(..)
            .map(|t| (rank.get(&t.path).copied().unwrap_or(newest + t.id), t))
            .collect();
        keyed.sort_by_key(|(k, _)| *k);
        target.tracks = keyed
            .into_iter()
            .enumerate()
            .map(|(i, (_, mut t))| {
                t.id = i as i64;
                t
            })
            .collect();
    }
    if let Some(i) = store.iter().position(|p| p.name == LIKED) {
        let liked = store.remove(i);
        store.insert(0, liked);
    }
    added
}

// --- linked playlists ----------------------------------------------------------

/// The playlist id in a Spotify playlist link or URI, if it is one.
pub fn playlist_id(link: &str) -> Option<String> {
    let link = link.trim();
    let rest = link
        .split("playlist/")
        .nth(1)
        .or_else(|| link.strip_prefix("spotify:playlist:"))?;
    let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (id.len() == 22).then_some(id)
}

/// Read a Spotify playlist anonymously from its public embed page (the same
/// page sites use for Spotify widgets). The embed lists at most 100 tracks
/// and no album names; covers come from oEmbed like imported tracks.
pub fn fetch_linked(link: &str) -> Result<Playlist> {
    let id = playlist_id(link).context("not a Spotify playlist link")?;
    let html = String::from_utf8_lossy(&http_get(&format!("https://open.spotify.com/embed/playlist/{id}"))?)
        .into_owned();
    let start = html
        .find(r#"<script id="__NEXT_DATA__" type="application/json">"#)
        .context("embed page has no playlist data")?;
    let json = &html[start..];
    let json = &json[json.find('>').unwrap_or(0) + 1..];
    let json = &json[..json.find("</script>").context("embed page truncated")?];
    let v: serde_json::Value = serde_json::from_str(json)?;
    let entity = &v["props"]["pageProps"]["state"]["data"]["entity"];
    let name = entity["name"].as_str().unwrap_or("Spotify playlist").to_string();
    let tracks = entity["trackList"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let tid = spotify_id(t["uri"].as_str()?)?;
            Some(Track {
                id: i as i64,
                path: audio_dir().join(format!("{tid}.m4a")),
                title: t["title"].as_str()?.to_string(),
                track_artist: t["subtitle"].as_str().unwrap_or("").to_string(),
                album_artist: t["subtitle"].as_str().unwrap_or("").to_string(),
                album: String::new(),
                year: None,
                track_no: None,
                disc_no: None,
                length_secs: t["duration"].as_u64().unwrap_or(0) / 1000,
            })
        })
        .collect();
    Ok(Playlist { name, tracks })
}

// --- YouTube audio ---------------------------------------------------------

/// The YouTube video a track was matched to, and videos rejected for it.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Match {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rejected: Vec<String>,
}

fn matches() -> &'static Mutex<HashMap<String, Match>> {
    static MATCHES: OnceLock<Mutex<HashMap<String, Match>>> = OnceLock::new();
    MATCHES.get_or_init(|| {
        let m = std::fs::read_to_string(config::spotify_matches_path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        Mutex::new(m)
    })
}

/// Apply `f` to a track's match record and persist the whole table.
fn update_match(id: &str, f: impl FnOnce(&mut Match)) {
    let Ok(mut m) = matches().lock() else { return };
    f(m.entry(id.to_string()).or_default());
    let path = config::spotify_matches_path();
    let _ = config::ensure_parent(&path);
    if let Ok(body) = toml::to_string(&*m) {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// One search result: a YouTube video that might be the track.
struct Candidate {
    id: String,
    title: String,
    channel: String,
    duration: Option<f64>,
    /// From a YouTube Music "songs" search: audio-only uploads, never music
    /// videos with intros, skits or sound effects cut into the song.
    song: bool,
}

/// Title words that mark a version other than the plain recording. Ignored
/// when the Spotify title itself contains them (e.g. a live album).
const OFF_VERSIONS: &[&str] = &[
    "official video", "music video", "official mv", "(video)", "[video]", "(mv)", "[mv]",
    "live", "cover", "remix", "karaoke", "instrumental", "8d", "slowed", "sped up",
    "nightcore", "reverb", "extended", "teaser", "trailer", "reaction",
];

/// Lower is better: songs-search results first, in their search order;
/// general results after, penalized for looking like a video or a different
/// version, or for a length that doesn't match the export's.
fn score(c: &Candidate, rank: usize, track: &Track) -> i64 {
    let mut s = rank as i64 + if c.song { 0 } else { 100 };
    let title = c.title.to_lowercase();
    let want = track.title.to_lowercase();
    // The song's name without Spotify's " - 2011 Remaster" / "(feat. …)"
    // suffixes; a candidate missing it is probably a different song.
    let core = want.split(" - ").next().unwrap_or(&want);
    let core = core.split(" (").next().unwrap_or(core).trim();
    if !core.is_empty() && !title.contains(core) {
        s += 30;
    }
    for w in OFF_VERSIONS {
        if title.contains(w) && !want.contains(w) {
            s += 50;
        }
    }
    if title.contains("audio") || c.channel.ends_with(" - Topic") {
        s -= 20;
    }
    if let Some(d) = c.duration
        && track.length_secs > 0
    {
        let diff = (d - track.length_secs as f64).abs();
        if diff > 60.0 {
            s += 100;
        } else if diff > 15.0 {
            s += 40;
        }
    }
    s
}

/// List up to five candidates from one search source.
fn search(source: &str, song: bool) -> Vec<Candidate> {
    list_videos(source, song, 5)
}

fn list_videos(source: &str, song: bool, count: usize) -> Vec<Candidate> {
    let Ok(out) = Command::new("yt-dlp")
        .args(["--flat-playlist", "--no-warnings", "--playlist-items"])
        .arg(format!("1:{count}"))
        .arg("--print")
        .arg("%(id)s\t%(title)s\t%(channel)s\t%(duration)s")
        .arg(source)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.trim().to_string();
            (id.len() == 11).then(|| Candidate {
                id,
                title: f.next().unwrap_or("").to_string(),
                channel: f.next().unwrap_or("").to_string(),
                duration: f.next().and_then(|d| d.parse().ok()),
                song,
            })
        })
        .collect()
}

/// The YouTube video for a track: its own (YouTube Music results), the one
/// it was matched to when fetched, or the top YouTube Music songs result.
pub fn video_for(track: &Track) -> Option<String> {
    let id = track_id(&track.path);
    if let Some(v) = id.as_deref().and_then(|i| i.strip_prefix("yt-")) {
        return Some(v.to_string());
    }
    if let Some(v) = id.as_deref().and_then(|i| matches().lock().ok()?.get(i)?.source.clone()) {
        return Some(v);
    }
    let query = format!("{} {}", track.track_artist, track.title);
    let url = format!("https://music.youtube.com/search?q={}#songs", url_encode(&query));
    search(&url, true).into_iter().next().map(|c| c.id)
}

/// One song of a YouTube Music radio: video id, cleaned-up artist and title.
pub struct RadioSong {
    pub video: String,
    pub artist: String,
    pub title: String,
}

/// YouTube Music's "start radio" mix for a video: songs picked for a
/// similar sound and mood, not just the same artist. Anonymous.
pub fn youtube_radio(video: &str, count: usize) -> Vec<RadioSong> {
    let url = format!("https://music.youtube.com/watch?v={video}&list=RDAMVM{video}");
    list_videos(&url, true, count + 1)
        .into_iter()
        .filter(|c| c.id != video)
        .map(|c| {
            let channel = c.channel.trim_end_matches(" - Topic").trim().to_string();
            let title = strip_video_noise(&c.title);
            // Label/fan uploads are titled "Artist - Song"; an artist's own
            // ("Topic") uploads are just the song.
            let topic = c.channel.ends_with(" - Topic");
            let quoted = title.split('"').map(str::trim).collect::<Vec<_>>();
            let (artist, title) = match title.split_once(" - ") {
                Some((a, t)) if !topic => (a.trim().to_string(), t.trim().to_string()),
                // Fan uploads: `Artist "Song"`.
                _ if !topic && quoted.len() >= 3 && !quoted[0].is_empty() => (quoted[0].to_string(), quoted[1].to_string()),
                _ => (channel, title),
            };
            RadioSong { video: c.id, artist, title }
        })
        .collect()
}

/// Drop "(Official Audio)", "[Lyric Video]" and the like from a video title.
fn strip_video_noise(title: &str) -> String {
    const NOISE: [&str; 8] = ["official", "audio", "video", "lyric", "visualizer", "visualiser", "hd", "4k"];
    let mut out = String::new();
    let mut rest = title;
    while let Some(open) = rest.find(['(', '[']) {
        let close = if rest[open..].starts_with('(') { ')' } else { ']' };
        let Some(len) = rest[open..].find(close) else { break };
        let inner = rest[open + 1..open + len].to_lowercase();
        out.push_str(&rest[..open]);
        if !inner.split(|c: char| !c.is_alphanumeric()).any(|w| NOISE.contains(&w)) {
            out.push_str(&rest[open..=open + len]);
        }
        rest = &rest[open + len + 1..];
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remember that `track` should be fetched from `video` (a radio pick is
/// already a specific upload, so no search is needed later).
pub fn set_source(track: &Track, video: &str) {
    if let Some(id) = track_id(&track.path) {
        update_match(&id, |m| {
            if m.source.is_none() && !m.rejected.iter().any(|r| r == video) {
                m.source = Some(video.to_string());
            }
        });
    }
}

/// Download `track`'s audio from YouTube (blocking), file it into the
/// library tree, and return the track with its metadata filled in (length,
/// album artist, numbering, year). With `reject`, the currently matched video
/// is first added to the track's rejected list (and its file deleted), so a
/// different candidate is chosen.
pub fn fetch_audio(track: &Track, reject: bool) -> Result<Track> {
    let dir = audio_dir();
    std::fs::create_dir_all(dir)?;
    let id = track_id(&track.path).context("bad track path")?;
    // A YouTube Music result is its own source: no searching, nothing to
    // reject in favor of.
    if let Some(video) = id.strip_prefix("yt-") {
        if reject {
            bail!("this track is a specific YouTube video");
        }
        let file = yt_dlp(&format!("https://www.youtube.com/watch?v={video}"), &dir.join(format!("{id}.dl.%(ext)s")))?;
        return file_into_library(&file, track);
    }
    let known = matches().lock().ok().and_then(|m| m.get(&id).cloned()).unwrap_or_default();
    // Already matched (an earlier fetch, or a radio pick): use that video.
    if !reject
        && let Some(video) = &known.source
        && let Ok(file) = yt_dlp(&format!("https://www.youtube.com/watch?v={video}"), &dir.join(format!("{id}.dl.%(ext)s")))
    {
        return file_into_library(&file, track);
    }
    let query = format!("{} {}", track.track_artist, track.title);
    let mut candidates = search(
        &format!("https://music.youtube.com/search?q={}#songs", url_encode(&query)),
        true,
    );

    if reject {
        // Files fetched before matches were recorded came from the top songs
        // result, so that's what's being rejected.
        let current = known.source.clone().or_else(|| candidates.first().map(|c| c.id.clone()));
        update_match(&id, |m| {
            m.rejected.extend(current);
            m.source = None;
        });
        remove_fetched(&track.path);
    }
    let rejected = matches()
        .lock()
        .ok()
        .and_then(|m| m.get(&id).map(|m| m.rejected.clone()))
        .unwrap_or_default();
    candidates.retain(|c| !rejected.contains(&c.id));
    // General search only when the songs search came up empty or found only
    // off versions (it's slower and mostly music videos).
    let ranked = |cs: &[Candidate]| cs.iter().enumerate().map(|(i, c)| score(c, i, track)).min();
    if ranked(&candidates).is_none_or(|best| best >= 50) {
        let mut more = search(
            &format!("ytsearch5:{} - {} audio", track.track_artist, track.title),
            false,
        );
        more.retain(|c| !rejected.contains(&c.id) && !candidates.iter().any(|k| k.id == c.id));
        candidates.extend(more);
    }
    if candidates.is_empty() {
        bail!(if rejected.is_empty() { "no match found" } else { "no matches left (all rejected)" });
    }
    let mut order: Vec<(i64, &Candidate)> = candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (score(c, i, track), c))
        .collect();
    order.sort_by_key(|(s, _)| *s);

    let template = dir.join(format!("{id}.dl.%(ext)s"));
    let mut last_err = String::new();
    for (_, c) in order.iter().take(3) {
        let url = format!("https://www.youtube.com/watch?v={}", c.id);
        match yt_dlp(&url, &template) {
            Ok(file) => {
                update_match(&id, |m| m.source = Some(c.id.clone()));
                return file_into_library(&file, track);
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    bail!("{last_err}")
}

/// Run yt-dlp on one video, returning the downloaded file's path.
fn yt_dlp(source: &str, template: &Path) -> Result<PathBuf> {
    let out = Command::new("yt-dlp")
        .args([
            "--quiet",
            "--no-warnings",
            "--no-progress",
            "--no-playlist",
            // YouTube's best stream is Opus (~140 kbps); AAC only as a
            // fallback. `-x` copies the audio out of its container
            // unchanged (webm → .opus), with no re-encoding.
            "-f",
            "bestaudio[acodec=opus]/bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]",
            "-x",
            "--print",
            "after_move:filepath",
            "-o",
        ])
        .arg(template)
        .arg(source)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow::anyhow!("yt-dlp not found in PATH"),
            _ => e.into(),
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(file) = stdout.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
        let file = PathBuf::from(file);
        if out.status.success() && file.exists() {
            return Ok(file);
        }
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let msg = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("download failed");
    bail!("{}", msg.trim().trim_start_matches("ERROR: "))
}

/// Write `track`'s metadata (and cover, if any) to the audio file at `file`,
/// replacing whatever tags it had.
fn tag_file(file: &Path, track: &Track, cover: Option<Vec<u8>>) -> Result<()> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::{Accessor, TagExt};
    use lofty::tag::{ItemKey, Tag};

    let tagged = lofty::read_from_path(file)?;
    let mut tag = Tag::new(tagged.primary_tag_type());
    tag.set_title(track.title.clone());
    tag.set_artist(track.track_artist.clone());
    tag.set_album(track.album.clone());
    tag.insert_text(ItemKey::AlbumArtist, track.album_artist.clone());
    if let Some(n) = track.track_no {
        tag.set_track(n);
    }
    if let Some(n) = track.disc_no {
        tag.set_disk(n);
    }
    let id = track_id(&track.path).unwrap_or_default();
    let extra = extra_for(&id);
    if let Some(date) = extra.release.or_else(|| track.year.map(|y| y.to_string())) {
        tag.insert_text(ItemKey::RecordingDate, date);
    }
    if let Some(isrc) = extra.isrc {
        tag.insert_text(ItemKey::Isrc, isrc);
    }
    if id.len() == 22 {
        tag.insert_text(ItemKey::Comment, format!("spotify:track:{id}"));
    }
    if let Some(bytes) = cover {
        tag.push_picture(
            Picture::unchecked(bytes)
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
    }
    tag.save_to_path(file, WriteOptions::default())?;
    Ok(())
}

/// Fill in what the export lacks from Deezer: the album artist (so
/// compilations file together), track/disc numbers and release year.
/// Matched by ISRC when known, else by an exact artist + title search.
fn enrich(track: &Track) -> Track {
    let mut t = track.clone();
    let id = track_id(&track.path).unwrap_or_default();
    let extra = extra_for(&id);
    let found = match (&extra.isrc, id.strip_prefix("dz")) {
        (_, Some(dz)) => crate::deezer::by_id(dz),
        (Some(isrc), _) => crate::deezer::by_isrc(isrc),
        _ => crate::deezer::by_name(&track.track_artist, &track.title),
    };
    if let Some(d) = found {
        t.album_artist = d.album_artist;
        t.track_no = t.track_no.or(d.track_no);
        t.disc_no = t.disc_no.or(d.disc_no);
        t.year = t.year.or(d.year);
        if t.album.is_empty() {
            t.album = d.album;
        }
    } else if t.album_artist.is_empty() || t.album_artist.contains(',') {
        // Without a lookup, the first credited artist is the best guess.
        t.album_artist = t.track_artist.split(',').next().unwrap_or("").trim().to_string();
    }
    t
}

/// A path component in the style of the user's library: `/` becomes `⁄`,
/// `:` becomes `_`, and trailing dots/spaces are dropped.
fn path_part(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| *c != '\0')
        .map(|c| match c {
            '/' => '⁄',
            ':' => '_',
            c => c,
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    while out.len() > 180 {
        out.pop();
    }
    if out.is_empty() { "Unknown".into() } else { out }
}

/// Library-tree location for a track: `Album Artist/[Year] Album/NN Title.m4a`,
/// with various-artist albums under "Compilations and Soundtracks".
pub fn library_path(t: &Track) -> PathBuf {
    let artist = if t.album_artist.eq_ignore_ascii_case("various artists") {
        "Compilations and Soundtracks".to_string()
    } else {
        path_part(&t.album_artist)
    };
    let album = if t.album.is_empty() { "Singles".to_string() } else { path_part(&t.album) };
    let album = match t.year {
        Some(y) => format!("[{y}] {album}"),
        None => album,
    };
    let title = path_part(&t.title);
    let file = match (t.disc_no, t.track_no) {
        (Some(d), Some(n)) if d > 1 => format!("{d}-{n:02} {title}.m4a"),
        (_, Some(n)) => format!("{n:02} {title}.m4a"),
        _ => format!("{title}.m4a"),
    };
    music_root().join(artist).join(album).join(file)
}

/// Tag a freshly downloaded (or re-filed) audio file, move it into the
/// library tree, and point the track's per-id path at it. The per-id
/// symlink is swapped in last, so the track only counts as fetched once its
/// file is complete and tagged.
fn file_into_library(file: &Path, track: &Track) -> Result<Track> {
    let mut t = enrich(track);
    let _ = tag_file(file, &t, cover_bytes(&track.path));
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("m4a").to_string();
    let mut dest = library_path(&t).with_extension(&ext);
    // A file already there that isn't this track's own (re-filed) copy is a
    // different track with the same name.
    let own = std::fs::read_link(&track.path).ok();
    if dest.exists() && own.as_deref() != Some(dest.as_path()) {
        // Two different tracks with the same name: keep both.
        let id = track_id(&track.path).unwrap_or_default();
        let stem = dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        dest.set_file_name(format!("{stem} [{id}].{ext}"));
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::rename(file, &dest)?;
    let link = track.path.with_extension("link");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&dest, &link)?;
    std::fs::rename(&link, &track.path)?;
    t.length_secs = scan::read_length(&dest).unwrap_or(t.length_secs);
    Ok(t)
}

/// Delete a fetched track: its library file (removing emptied album/artist
/// folders) and the per-id link.
fn remove_fetched(path: &Path) {
    if let Ok(real) = std::fs::read_link(path) {
        let _ = std::fs::remove_file(&real);
        let root = music_root();
        let mut dir = real.parent();
        while let Some(d) = dir {
            if d == root || !d.starts_with(&root) || std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Fetched files at per-id paths that no known track points at (songs
/// played from Explore and never saved, or since removed from the library),
/// described by their own tags so they can be filed too.
pub fn orphan_files(known: &std::collections::HashSet<PathBuf>) -> Vec<Track> {
    std::fs::read_dir(audio_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            // Skip in-progress downloads (`<id>.dl.m4a`) and links.
            let plain = p.file_stem().is_some_and(|s| !s.to_string_lossy().contains('.'));
            plain && !known.contains(p) && std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
        })
        .filter_map(|p| scan::read_track(&p))
        .collect()
}

/// Bring already-fetched files up to date: files saved before the library
/// tree existed (plain files at the per-id path) are re-tagged and filed,
/// and links whose file has gone are removed so the track can be re-fetched.
/// Returns the tracks whose metadata changed. Each file is filed from a copy
/// and the original only replaced by the link afterwards, so a track that is
/// playing is never modified underneath the decoder.
pub fn organize_fetched(tracks: Vec<Track>) -> Vec<Track> {
    let mut changed = Vec::new();
    for track in tracks {
        let Ok(meta) = std::fs::symlink_metadata(&track.path) else { continue };
        if meta.file_type().is_symlink() {
            if !track.path.exists() {
                let _ = std::fs::remove_file(&track.path);
            }
            continue;
        }
        let ext = std::fs::canonicalize(&track.path)
            .ok()
            .and_then(|p| p.extension().map(|e| e.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "m4a".into());
        let tmp = track.path.with_extension(format!("refile.{ext}"));
        if std::fs::copy(&track.path, &tmp).is_ok()
            && let Ok(t) = file_into_library(&tmp, &track)
        {
            changed.push(t);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    changed
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// --- covers ----------------------------------------------------------------

/// A Spotify track's album cover, decoded.
pub fn load_cover(path: &Path) -> Option<DynamicImage> {
    let embedded = if path.exists() { scan::read_cover(path) } else { None };
    let bytes = match embedded {
        Some(b) => b,
        None => {
            let b = url_bytes(&cover_url(path)?)?;
            // Fetched before its cover could be found (offline, rate
            // limited): embed it now that it's here.
            if path.exists() {
                let _ = embed_cover(path, &b);
            }
            b
        }
    };
    image::load_from_memory(&bytes).ok()
}

/// Add a cover to a fetched track's file, keeping its other tags. Written
/// to a copy that then replaces the file, so a decoder reading it (the song
/// may be playing) keeps its own unchanged copy.
fn embed_cover(path: &Path, bytes: &[u8]) -> Result<()> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::tag::{Tag, TagExt};

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let real = std::fs::canonicalize(path)?;
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ext = real.extension().and_then(|e| e.to_str()).unwrap_or("m4a");
    let tmp = real.with_extension(format!("cover{}-{n}.{ext}", std::process::id()));
    std::fs::copy(&real, &tmp)?;
    let result = (|| -> Result<()> {
        let tagged = lofty::read_from_path(&tmp)?;
        let mut tag = tagged.primary_tag().cloned().unwrap_or_else(|| Tag::new(tagged.primary_tag_type()));
        if tag.pictures().is_empty() {
            let mime = if bytes.starts_with(b"\x89PNG") { MimeType::Png } else { MimeType::Jpeg };
            tag.push_picture(
                Picture::unchecked(bytes.to_vec()).pic_type(PictureType::CoverFront).mime_type(mime).build(),
            );
            tag.save_to_path(&tmp, WriteOptions::default())?;
        }
        std::fs::rename(&tmp, &real)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A Spotify track's album cover as image bytes. A fetched track has it
/// embedded. Otherwise the cover's URL is looked up once (the export's image
/// URL, Deezer, YouTube, or an anonymous Spotify oEmbed lookup) and kept as a
/// tiny `<id>.url` note; the image itself is cached by URL, so every track of
/// an album shares one file.
fn cover_bytes(path: &Path) -> Option<Vec<u8>> {
    if path.exists()
        && let Some(raw) = scan::read_cover(path)
    {
        return Some(raw);
    }
    url_bytes(&cover_url(path)?)
}

fn cover_url(path: &Path) -> Option<String> {
    let id = track_id(path)?;
    let note = covers_dir().join(format!("{id}.url"));
    if let Ok(url) = std::fs::read_to_string(&note) {
        return Some(url.trim().to_string());
    }
    let url = match extra_for(&id).cover_url {
        Some(u) => u,
        None if id.starts_with("dz") => crate::deezer::track_cover(&id[2..])?,
        None if id.starts_with("yt-") => format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", &id[3..]),
        // Hashed/ISRC ids aren't Spotify ids, so oEmbed can't resolve them.
        None if id.len() == 22 => oembed_thumbnail(&id)?,
        None => return None,
    };
    let _ = std::fs::create_dir_all(covers_dir());
    let _ = std::fs::write(&note, &url);
    Some(url)
}

/// Where a cover comes from: a track (embedded once fetched, else looked up)
/// or an image URL (Explore albums/artists/playlists).
#[derive(Clone)]
pub enum CoverSrc {
    Track(PathBuf),
    Url(String),
}

/// Download every cover in `srcs` to the disk cache in the background, a
/// few at a time, so a whole list's thumbnails are ready before it's
/// scrolled. Stops early once `generation` moves past `mine` (the user
/// opened a different list).
pub fn prefetch_covers(srcs: Vec<CoverSrc>, generation: std::sync::Arc<std::sync::atomic::AtomicU64>, mine: u64) {
    use std::sync::atomic::Ordering;
    let queue = std::sync::Arc::new(Mutex::new(std::collections::VecDeque::from(srcs)));
    for _ in 0..4 {
        let queue = queue.clone();
        let generation = generation.clone();
        std::thread::spawn(move || {
            while generation.load(Ordering::Relaxed) == mine {
                let Some(src) = queue.lock().ok().and_then(|mut q| q.pop_front()) else { break };
                match src {
                    CoverSrc::Track(p) if !p.exists() => {
                        if let Some(url) = cover_url(&p) {
                            let _ = url_bytes(&url);
                        }
                    }
                    CoverSrc::Url(u) if !u.is_empty() => {
                        let _ = url_bytes(&u);
                    }
                    _ => {}
                }
            }
        });
    }
}

/// The album-art URL Spotify's public oEmbed endpoint reports for a track,
/// upgraded from its 300px default to the 640px rendition.
fn oembed_thumbnail(id: &str) -> Option<String> {
    let url = format!("https://open.spotify.com/oembed?url=https://open.spotify.com/track/{id}");
    let body = http_get(&url).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let thumb = v.get("thumbnail_url")?.as_str()?;
    Some(thumb.replace("ab67616d00001e02", "ab67616d0000b273"))
}

/// An image from a URL, cached on disk by a hash of the URL.
pub fn url_image(url: &str) -> Option<DynamicImage> {
    image::load_from_memory(&url_bytes(url)?).ok()
}

/// The bytes of an image URL, from the on-disk cache or downloaded into it.
fn url_bytes(url: &str) -> Option<Vec<u8>> {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(url, &mut h);
    let file = covers_dir().join(format!("url-{:016x}.jpg", std::hash::Hasher::finish(&h)));
    if let Ok(bytes) = std::fs::read(&file) {
        return Some(bytes);
    }
    let bytes = http_get(url).ok()?;
    image::load_from_memory(&bytes).ok()?;
    let _ = std::fs::create_dir_all(covers_dir());
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &file);
    }
    Some(bytes)
}

/// Delete per-track cover images from before covers were shared per album
/// (`<id>.jpg`); URL-keyed images (`url-….jpg`) and `.url` notes are kept.
pub fn prune_legacy_covers() {
    for e in std::fs::read_dir(covers_dir()).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jpg") && !name.starts_with("url-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// GET a URL. Requests to each host are spaced out (Deezer allows ~50
/// requests per 5 s; Spotify's public pages are stricter), and a 429 "too
/// many requests" is retried after the wait the server asks for.
pub fn http_get(url: &str) -> Result<Vec<u8>> {
    let host = url.split('/').nth(2).unwrap_or("").to_string();
    let gap = match host.as_str() {
        "api.deezer.com" => Duration::from_millis(130),
        "open.spotify.com" => Duration::from_millis(400),
        _ => Duration::from_millis(40),
    };
    for attempt in 0..3 {
        throttle(&host, gap);
        match ureq::get(url).call() {
            Ok(mut resp) => return Ok(resp.body_mut().read_to_vec()?),
            Err(ureq::Error::StatusCode(429)) if attempt < 2 => {
                // Make every request to this host wait, not just this one.
                throttle(&host, Duration::from_secs(5));
            }
            Err(e) => return Err(e.into()),
        }
    }
    bail!("rate limited by {host}")
}

/// Wait until `host` was last requested at least `gap` ago, then claim the
/// next slot. Slots are handed out under the lock, so concurrent callers
/// queue up instead of all firing at once.
fn throttle(host: &str, gap: Duration) {
    static NEXT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let wait = {
        let mut next = NEXT.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let slot = next.get(host).copied().filter(|t| *t > now).unwrap_or(now);
        next.insert(host.to_string(), slot + gap);
        slot - now
    };
    std::thread::sleep(wait);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_tunemymusic_and_exportify_csvs() {
        let dir = std::env::temp_dir().join(format!("musictui_sp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Library.csv"),
            "Track name,Artist name,Album,Playlist name,Type,ISRC,Spotify - id\n\
             Windy,The Association,Anthology,Favorite Songs,Favorite,USWB19901626,6RUpOheT93fUdI508K5w6U\n\
             \"Archie, Marry Me\",Alvvays,Alvvays,Cuddle,Playlist,US3R41428202,5HLes17mwCwKt81mi0Tk35\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("Road_Trip.csv"),
            "Track URI,Track Name,Artist Name(s),Album Name,Album Release Date,Album Image URL,Track Duration (ms)\n\
             spotify:track:0Y2SrByf4G3kbq2nBEHQRn,Spirit in the Sky,Norman Greenbaum,Spirit,1969-01-01,https://i.scdn.co/image/x,242000\n",
        )
        .unwrap();

        let lists = import(&dir).unwrap();
        let names: Vec<&str> = lists.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, [LIKED, "Cuddle", "Road Trip"]);
        assert_eq!(lists[1].tracks[0].title, "Archie, Marry Me");
        let road = &lists[2].tracks[0];
        assert_eq!(road.length_secs, 242);
        assert_eq!(road.year, Some(1969));
        assert!(road.path.ends_with("0Y2SrByf4G3kbq2nBEHQRn.m4a"));
        assert!(is_remote(&road.path));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_keeps_local_changes_and_appends_new_rows_as_newest() {
        let t = |id: &str, pos: i64| Track {
            id: pos,
            path: audio_dir().join(format!("{id}.m4a")),
            title: id.into(),
            track_artist: String::new(),
            album_artist: String::new(),
            album: String::new(),
            year: None,
            track_no: None,
            disc_no: None,
            length_secs: 0,
        };
        let mut store = vec![Playlist { name: LIKED.into(), tracks: vec![t("a", 0), t("liked_locally", 1)] }];
        let import = vec![
            Playlist { name: "Road".into(), tracks: vec![t("r", 0)] },
            Playlist { name: LIKED.into(), tracks: vec![t("a", 0), t("b", 1), t("c", 2)] },
        ];
        assert_eq!(merge_import(&mut store, import), 3);
        let liked: Vec<(&str, i64)> = store[0].tracks.iter().map(|x| (x.title.as_str(), x.id)).collect();
        assert_eq!(liked, [("a", 0), ("b", 1), ("c", 2), ("liked_locally", 3)]);
        assert_eq!(store[1].name, "Road");
        assert_eq!(merge_import(&mut store, vec![]), 0);
    }

    #[test]
    fn exportify_folder_orders_by_added_at_and_skips_album_files() {
        let dir = std::env::temp_dir().join(format!("musictui_exp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Liked_Songs.csv"),
            "Track URI,Track Name,Artist Name(s),Added At\n\
             spotify:track:6RUpOheT93fUdI508K5w6U,Newest,A,2026-10-01T10:00:00Z\n\
             spotify:track:0Y2SrByf4G3kbq2nBEHQRn,Oldest,B,2019-01-01T10:00:00Z\n",
        )
        .unwrap();
        std::fs::write(dir.join("saved_albums.csv"), "Album URI,Album Name\nspotify:album:x,Y\n").unwrap();
        let lists = import(&dir).unwrap();
        assert_eq!(lists.len(), 1);
        let ranks: Vec<(&str, i64)> = lists[0].tracks.iter().map(|t| (t.title.as_str(), t.id)).collect();
        assert_eq!(ranks, [("Oldest", 0), ("Newest", 1)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn library_paths_follow_the_music_folder_layout() {
        let t = |artist: &str, album: &str, year, disc, no, title: &str| Track {
            id: 0,
            path: PathBuf::new(),
            title: title.into(),
            track_artist: artist.into(),
            album_artist: artist.into(),
            album: album.into(),
            year,
            track_no: no,
            disc_no: disc,
            length_secs: 0,
        };
        let rel = |t: &Track| library_path(t).strip_prefix(music_root()).unwrap().to_string_lossy().into_owned();
        assert_eq!(rel(&t("AC/DC", "Back in Black", Some(1980), Some(1), Some(6), "Back in Black")),
            "AC⁄DC/[1980] Back in Black/06 Back in Black.m4a");
        assert_eq!(rel(&t("Various Artists", "Saturday Night Fever: OST", None, Some(2), Some(3), "Night Fever")),
            "Compilations and Soundtracks/Saturday Night Fever_ OST/2-03 Night Fever.m4a");
        assert_eq!(rel(&t("Bush", "", None, None, None, "Machinehead...")), "Bush/Singles/Machinehead.m4a");
    }

    #[test]
    fn radio_titles_lose_video_noise() {
        assert_eq!(strip_video_noise("Malo - Suavecito (Official Audio)"), "Malo - Suavecito");
        assert_eq!(strip_video_noise("Apache [HD] (2007 Remaster)"), "Apache (2007 Remaster)");
        assert_eq!(strip_video_noise("Song (Lyric Video) [Visualizer]"), "Song");
    }

    #[test]
    fn playlist_links_parse() {
        let id = "37i9dQZEVXcJZyENOWUFo7";
        assert_eq!(playlist_id(&format!("https://open.spotify.com/playlist/{id}?si=abc123")).as_deref(), Some(id));
        assert_eq!(playlist_id(&format!("spotify:playlist:{id}")).as_deref(), Some(id));
        assert_eq!(playlist_id("https://open.spotify.com/track/6RUpOheT93fUdI508K5w6U"), None);
    }

    #[test]
    fn ranking_prefers_plain_audio_over_videos_and_other_songs() {
        let track = Track {
            id: 0,
            path: PathBuf::from("x.m4a"),
            title: "Windy - 2011 Remaster".into(),
            track_artist: "The Association".into(),
            album_artist: String::new(),
            album: String::new(),
            year: None,
            track_no: None,
            disc_no: None,
            length_secs: 0,
        };
        let c = |title: &str, song: bool| Candidate {
            id: "aaaaaaaaaaa".into(),
            title: title.into(),
            channel: String::new(),
            duration: None,
            song,
        };
        let audio = score(&c("Windy", true), 0, &track);
        let mv = score(&c("Windy (Official Music Video)", false), 0, &track);
        let other = score(&c("Cherish", true), 0, &track);
        assert!(audio < other && other < mv);
    }
}
