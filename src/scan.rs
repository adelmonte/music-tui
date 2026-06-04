use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Accessor;
use lofty::tag::ItemKey;
use walkdir::WalkDir;

use crate::model::Track;

/// Audio extensions we attempt to read. Decoding support depends on rodio's
/// symphonia features; tag reading depends on lofty.
const AUDIO_EXTS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "wav", "m4a", "mp4", "aac", "alac", "aiff", "aif",
];

/// Messages streamed from the background scan thread to the UI event loop.
pub enum ScanMsg {
    Progress { done: usize, total: usize },
    Finished(Vec<Track>),
    /// Reserved for surfacing scan failures; handled by the UI poll loop.
    #[allow(dead_code)]
    Error(String),
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Spawns a thread that walks `root`, reads tags, and reports progress + the
/// final track list over `tx`. Returns immediately.
pub fn spawn(root: PathBuf, tx: Sender<ScanMsg>) {
    thread::spawn(move || {
        let tracks = scan_dir(&root, |done, total| {
            let _ = tx.send(ScanMsg::Progress { done, total });
        });
        let _ = tx.send(ScanMsg::Finished(tracks));
    });
}

/// Walks `root`, reads tags from every audio file, and returns the tracks.
/// `progress(done, total)` is called periodically. Runs on the caller's thread.
pub fn scan_dir(root: &Path, mut progress: impl FnMut(usize, usize)) -> Vec<Track> {
    let files: Vec<PathBuf> = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        .filter(|p| p.is_file() && is_audio(p))
        .collect();

    let total = files.len();
    let mut tracks = Vec::with_capacity(total);
    for (i, path) in files.into_iter().enumerate() {
        if let Some(t) = read_track(&path) {
            tracks.push(t);
        }
        if i % 25 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }
    tracks
}

fn read_track(path: &Path) -> Option<Track> {
    let tagged = Probe::open(path).ok()?.read().ok()?;
    let duration = tagged.properties().duration().as_secs();
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let stem = || {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Unknown".into())
    };

    let (title, track_artist, album, album_artist, year, track_no, disc_no) = match tag {
        Some(tag) => {
            let title = tag.title().map(|s| s.to_string()).unwrap_or_else(stem);
            let track_artist = tag
                .artist()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Unknown Artist".into());
            let album = tag
                .album()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "Unknown Album".into());
            let album_artist = tag
                .get_string(ItemKey::AlbumArtist)
                .map(|s| s.to_string())
                .unwrap_or_else(|| track_artist.clone());
            let year = parse_year(tag);
            (
                title,
                track_artist,
                album,
                album_artist,
                year,
                tag.track(),
                tag.disk(),
            )
        }
        None => (
            stem(),
            "Unknown Artist".into(),
            "Unknown Album".into(),
            "Unknown Artist".into(),
            None,
            None,
            None,
        ),
    };

    let album = strip_year_prefix(&album, year);

    Some(Track {
        id: 0,
        path: path.to_path_buf(),
        title,
        track_artist,
        album_artist,
        album,
        year,
        track_no,
        disc_no,
        length_secs: duration,
    })
}

/// Strip a leading `[YYYY]` or `(YYYY)` from an album string when the year
/// matches the track's date metadata, avoiding a doubled year in the UI.
fn strip_year_prefix(album: &str, year: Option<i32>) -> String {
    let Some(y) = year else { return album.to_string() };
    for (open, close) in [('[', ']'), ('(', ')')] {
        let prefix = format!("{open}{y}{close}");
        if let Some(rest) = album.strip_prefix(&prefix) {
            let stripped = rest.trim_start();
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
    }
    album.to_string()
}

fn parse_year(tag: &lofty::tag::Tag) -> Option<i32> {
    tag.get_string(ItemKey::Year)
        .or_else(|| tag.get_string(ItemKey::RecordingDate))
        .and_then(|s| s.get(0..4).and_then(|y| y.parse::<i32>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a minimal valid PCM WAV of `secs` seconds (mono, 8 kHz, 16-bit silence).
    fn write_wav(path: &Path, secs: u32) {
        let sample_rate = 8000u32;
        let n = sample_rate * secs;
        let data_len = n * 2;
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        f.write_all(&vec![0u8; data_len as usize]).unwrap();
    }

    #[test]
    fn scan_then_db_roundtrip() {
        let dir = std::env::temp_dir().join(format!("musictui_test_{}", std::process::id()));
        let sub = dir.join("Album");
        std::fs::create_dir_all(&sub).unwrap();
        write_wav(&sub.join("one.wav"), 2);
        write_wav(&sub.join("two.wav"), 1);
        // A non-audio file must be ignored.
        std::fs::write(dir.join("notes.txt"), b"hi").unwrap();

        let tracks = scan_dir(&dir, |_, _| {});
        assert_eq!(tracks.len(), 2, "should find 2 audio files");
        assert!(tracks.iter().any(|t| t.length_secs == 2));
        // No tags -> title falls back to file stem.
        assert!(tracks.iter().any(|t| t.title == "one"));

        // Round-trip through an in-memory SQLite index.
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::init(&conn).unwrap();
        crate::db::replace_all(&mut conn, &tracks).unwrap();
        let loaded = crate::db::load_all(&conn).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|t| t.id > 0), "ids assigned on load");

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Reads embedded cover art bytes from a file's tags, if present.
pub fn read_cover(path: &Path) -> Option<Vec<u8>> {
    let tagged = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let pics = tag.pictures();
    if pics.is_empty() {
        return None;
    }
    // Prefer an explicit front cover, otherwise take the first picture.
    let pic = pics
        .iter()
        .find(|p| p.pic_type() == lofty::picture::PictureType::CoverFront)
        .unwrap_or(&pics[0]);
    Some(pic.data().to_vec())
}
