use std::path::PathBuf;

use anyhow::Result;
use rusqlite::Connection;

use crate::config;
use crate::model::Track;

/// Opens (creating if needed) the SQLite library index and ensures the schema.
pub fn open() -> Result<Connection> {
    let path = config::db_path();
    config::ensure_parent(&path)?;
    let conn = Connection::open(&path)?;
    init(&conn)?;
    Ok(conn)
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracks (
            id           INTEGER PRIMARY KEY,
            path         TEXT NOT NULL UNIQUE,
            title        TEXT NOT NULL,
            track_artist TEXT NOT NULL,
            album_artist TEXT NOT NULL,
            album        TEXT NOT NULL,
            year         INTEGER,
            track_no     INTEGER,
            disc_no      INTEGER,
            length_secs  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_album_artist ON tracks(album_artist);
        CREATE INDEX IF NOT EXISTS idx_album ON tracks(album);
        CREATE INDEX IF NOT EXISTS idx_year ON tracks(year);",
    )?;
    Ok(())
}

/// Replaces the entire index with a freshly scanned track set.
pub fn replace_all(conn: &mut Connection, tracks: &[Track]) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM tracks", [])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO tracks
                (path, title, track_artist, album_artist, album, year, track_no, disc_no, length_secs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for t in tracks {
            stmt.execute(rusqlite::params![
                t.path.to_string_lossy(),
                t.title,
                t.track_artist,
                t.album_artist,
                t.album,
                t.year,
                t.track_no,
                t.disc_no,
                t.length_secs as i64,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Loads every track from the index into memory.
pub fn load_all(conn: &Connection) -> Result<Vec<Track>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, title, track_artist, album_artist, album, year, track_no, disc_no, length_secs
         FROM tracks",
    )?;
    let rows = stmt.query_map([], |r| {
        let path: String = r.get(1)?;
        Ok(Track {
            id: r.get(0)?,
            path: PathBuf::from(path),
            title: r.get(2)?,
            track_artist: r.get(3)?,
            album_artist: r.get(4)?,
            album: r.get(5)?,
            year: r.get(6)?,
            track_no: r.get(7)?,
            disc_no: r.get(8)?,
            length_secs: r.get::<_, i64>(9)? as u64,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
