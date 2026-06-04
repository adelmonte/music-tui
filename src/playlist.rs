use std::fs;

use serde::{Deserialize, Serialize};

use crate::config;
use crate::model::Track;

/// A named, ordered list of tracks. Tracks are stored in full so playlists
/// survive library rescans and edits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Playlist {
    pub name: String,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PlaylistFile {
    #[serde(default)]
    playlists: Vec<Playlist>,
}

/// Load saved playlists; returns an empty list if none exist or the file is bad.
pub fn load() -> Vec<Playlist> {
    let path = config::playlists_path();
    match fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<PlaylistFile>(&s)
            .map(|f| f.playlists)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist playlists to disk (best-effort).
pub fn save(playlists: &[Playlist]) {
    let path = config::playlists_path();
    let _ = config::ensure_parent(&path);
    let file = PlaylistFile {
        playlists: playlists.to_vec(),
    };
    if let Ok(s) = toml::to_string_pretty(&file) {
        let _ = fs::write(&path, s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn track(title: &str, year: Option<i32>, no: Option<u32>) -> Track {
        Track {
            id: 0,
            path: PathBuf::from(format!("/m/{title}.flac")),
            title: title.into(),
            track_artist: "Artist".into(),
            album_artist: "Artist".into(),
            album: "Album".into(),
            year,
            track_no: no,
            disc_no: None,
            length_secs: 180,
        }
    }

    #[test]
    fn playlists_round_trip_through_toml() {
        let file = PlaylistFile {
            playlists: vec![
                Playlist {
                    name: "Mix".into(),
                    tracks: vec![track("a", Some(1999), Some(1)), track("b", None, None)],
                },
                Playlist {
                    name: "Empty".into(),
                    tracks: vec![],
                },
            ],
        };
        let s = toml::to_string_pretty(&file).expect("playlists must serialize to TOML");
        let back: PlaylistFile = toml::from_str(&s).expect("playlists must parse back");
        assert_eq!(back.playlists.len(), 2);
        assert_eq!(back.playlists[0].tracks.len(), 2);
        assert_eq!(back.playlists[0].tracks[1].year, None);
        assert_eq!(back.playlists[1].tracks.len(), 0);
    }
}
