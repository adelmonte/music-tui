use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A single playable track, denormalized so the in-memory library can derive
/// the artist and album views without joins. `album_artist` is kept distinct
/// from `track_artist` so compilations group correctly under one album.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    /// Database row id (0 before persistence). Kept for future per-track ops.
    #[allow(dead_code)]
    pub id: i64,
    pub path: PathBuf,
    pub title: String,
    pub track_artist: String,
    pub album_artist: String,
    pub album: String,
    pub year: Option<i32>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub length_secs: u64,
}

/// The four searchable fields, lowercased and joined with a separator that no
/// query can contain, so `contains` over the blob matches each field
/// independently (a query never spans two fields) — equivalent to the old
/// per-field check but without re-lowercasing on every keystroke.
fn search_blob(t: &Track) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        t.title, t.track_artist, t.album, t.album_artist
    )
    .to_lowercase()
}

/// An album as shown in column 2, identified by (album_artist, title, year).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AlbumRef {
    pub album_artist: String,
    pub title: String,
    pub year: Option<i32>,
}

/// The whole library held in memory. The SQLite database is only a persistence
/// cache; all browsing/filtering happens here against `tracks`.
#[derive(Default)]
pub struct Library {
    pub tracks: Vec<Track>,
    /// Lowercased search blobs, parallel to `tracks` (same index), built once at
    /// construction so per-keystroke filtering is one allocation-free `contains`.
    /// Kept in sync by only ever building the library through `Library::new`.
    blobs: Vec<String>,
}

impl Library {
    pub fn new(tracks: Vec<Track>) -> Self {
        let blobs = tracks.iter().map(search_blob).collect();
        Self { tracks, blobs }
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Distinct album artists (column 1), alphabetical, honoring the search filter.
    /// When `sort_articles` is true, leading "The " is moved to the end for sorting
    /// (e.g. "The Doors" sorts as "Doors, The").
    pub fn album_artists(&self, query: &str, sort_articles: bool) -> Vec<String> {
        let mut v: Vec<String> = self
            .tracks
            .iter()
            .zip(&self.blobs)
            .filter(|(_, b)| query.is_empty() || b.contains(query))
            .map(|(t, _)| t.album_artist.clone())
            .collect();
        v.sort_by_key(|s| article_sort_key(s, sort_articles));
        v.dedup();
        v
    }

    /// Albums (column 2) for the selected artist, sorted by year then title.
    /// When `artist` is `None` every matching album is listed.
    pub fn albums(&self, query: &str, artist: Option<&str>) -> Vec<AlbumRef> {
        let mut v: Vec<AlbumRef> = self
            .tracks
            .iter()
            .zip(&self.blobs)
            .filter(|(_, b)| query.is_empty() || b.contains(query))
            .map(|(t, _)| t)
            .filter(|t| artist.is_none_or(|a| t.album_artist == a))
            .map(|t| AlbumRef {
                album_artist: t.album_artist.clone(),
                title: t.album.clone(),
                year: t.year,
            })
            .collect();
        v.sort_by(|a, b| {
            a.year
                .cmp(&b.year)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        v.dedup();
        v
    }

    /// A representative track for an album (first match by artist+title+year),
    /// used to source cover art for the Albums-column thumbnail.
    pub fn album_cover_track(&self, album: &AlbumRef) -> Option<&Track> {
        self.tracks.iter().find(|t| {
            t.album == album.title && t.album_artist == album.album_artist && t.year == album.year
        })
    }

    /// Tracks (column 3) filtered by the current artist/album selection and search.
    pub fn tracks_view(
        &self,
        query: &str,
        artist: Option<&str>,
        album: Option<&AlbumRef>,
    ) -> Vec<Track> {
        let mut v: Vec<Track> = self
            .tracks
            .iter()
            .zip(&self.blobs)
            .filter(|(_, b)| query.is_empty() || b.contains(query))
            .map(|(t, _)| t)
            .filter(|t| artist.is_none_or(|a| t.album_artist == a))
            .filter(|t| {
                album.is_none_or(|al| {
                    t.album == al.title && t.album_artist == al.album_artist && t.year == al.year
                })
            })
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            a.year
                .cmp(&b.year)
                .then_with(|| a.album.to_lowercase().cmp(&b.album.to_lowercase()))
                .then_with(|| a.disc_no.cmp(&b.disc_no))
                .then_with(|| a.track_no.cmp(&b.track_no))
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        v
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn next(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::All => "all",
            RepeatMode::One => "one",
        }
    }
}

/// Sort key that moves a leading "The " to the end: "The Doors" → "doors, the".
pub fn article_sort_key(s: &str, sort_articles: bool) -> String {
    if sort_articles
        && let Some(rest) = s.strip_prefix("The ").or_else(|| s.strip_prefix("the ")) {
            return format!("{}, the", rest.to_lowercase());
        }
    s.to_lowercase()
}

/// Display form: "The Doors" → "Doors, The" when sort_articles is on.
pub fn display_artist(s: &str, sort_articles: bool) -> String {
    if sort_articles
        && let Some(rest) = s.strip_prefix("The ").or_else(|| s.strip_prefix("the ")) {
            return format!("{}, The", rest);
        }
    s.to_string()
}

/// Format seconds as M:SS (or H:MM:SS for long tracks).
pub fn fmt_time(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn track(title: &str, ta: &str, aa: &str, album: &str, year: i32, no: u32) -> Track {
        Track {
            id: 0,
            path: PathBuf::from(format!("/m/{title}.flac")),
            title: title.into(),
            track_artist: ta.into(),
            album_artist: aa.into(),
            album: album.into(),
            year: Some(year),
            track_no: Some(no),
            disc_no: None,
            length_secs: 200,
        }
    }

    fn sample() -> Library {
        Library::new(vec![
            track("Song B", "Beck", "Beck", "Odelay", 1996, 2),
            track("Song A", "Beck", "Beck", "Odelay", 1996, 1),
            track("Older", "Beck", "Beck", "Mellow Gold", 1994, 1),
            // A compilation: album_artist differs from track artists.
            track("Guest One", "Artist X", "Various Artists", "Comp", 2001, 1),
            track("Guest Two", "Artist Y", "Various Artists", "Comp", 2001, 2),
        ])
    }

    #[test]
    fn album_artists_are_distinct_and_sorted() {
        let lib = sample();
        // "Beck" and "Various Artists" — track artists X/Y do NOT appear here.
        assert_eq!(lib.album_artists("", true), vec!["Beck", "Various Artists"]);
    }

    #[test]
    fn albums_filtered_by_artist_and_sorted_by_year() {
        let lib = sample();
        let albums = lib.albums("", Some("Beck"));
        let titles: Vec<_> = albums.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, vec!["Mellow Gold", "Odelay"]); // 1994 before 1996
    }

    #[test]
    fn tracks_view_respects_album_and_track_order() {
        let lib = sample();
        let album = AlbumRef {
            album_artist: "Beck".into(),
            title: "Odelay".into(),
            year: Some(1996),
        };
        let tracks = lib.tracks_view("", Some("Beck"), Some(&album));
        let titles: Vec<_> = tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Song A", "Song B"]); // sorted by track_no
    }

    #[test]
    fn search_filters_across_fields() {
        let lib = sample();
        // Matching a track artist that is not an album artist still surfaces the album.
        assert_eq!(lib.album_artists("artist x", true), vec!["Various Artists"]);
        assert_eq!(lib.tracks_view("guest two", None, None).len(), 1);
    }
}
