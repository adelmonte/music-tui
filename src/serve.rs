//! `music-tui serve`: the sync server. Per user it keeps the resume record
//! (see `sync`), and the library (likes and playlists) and podcasts (where
//! each episode stopped, finished ones, subscriptions) as one row per item, so
//! changes from different devices merge: the newest change to an item wins.
//! Users are told apart by a random key each device sends as a bearer token;
//! only a hash of the key is stored.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tiny_http::{Header, Method, Request, Response, Server};

const USAGE: &str = "\
usage: music-tui serve [--listen ADDR] [--db FILE]
       music-tui serve add-user NAME
       music-tui serve remove-user NAME
       music-tui serve users

The sync server. ADDR defaults to 0.0.0.0:7700 (or $MUSIC_TUI_LISTEN), FILE to
~/.local/share/music-tui/sync.db (or $MUSIC_TUI_SYNC_DB).";

/// The largest record accepted: a queue of many thousands of songs.
const MAX_BODY: usize = 32 << 20;

pub fn main(args: &[String]) -> Result<()> {
    let mut listen = std::env::var("MUSIC_TUI_LISTEN").unwrap_or_else(|_| "0.0.0.0:7700".into());
    let mut db = std::env::var_os("MUSIC_TUI_SYNC_DB").map(PathBuf::from).unwrap_or_else(crate::config::sync_db_path);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--listen" => listen = it.next().context("--listen needs an address")?.clone(),
            "--db" => db = PathBuf::from(it.next().context("--db needs a file")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => rest.push(a.as_str()),
        }
    }
    let conn = open(&db)?;
    match rest.as_slice() {
        [] => run(conn, &listen),
        ["add-user", name] => {
            let key = new_key()?;
            let added = conn.execute(
                "INSERT OR IGNORE INTO users (name, key_hash) VALUES (?1, ?2)",
                params![name, hash(&key)],
            )?;
            if added == 0 {
                bail!("there's already a user named {name}");
            }
            println!("{key}");
            eprintln!("Added {name}. Enter this key, with the server's address, in each of their apps.");
            Ok(())
        }
        ["remove-user", name] => {
            let n = conn.execute("DELETE FROM users WHERE name = ?1", params![name])?;
            for table in ["now", "lists", "songs", "eps", "subs", "versions"] {
                conn.execute(&format!("DELETE FROM {table} WHERE user = ?1"), params![name])?;
            }
            if n == 0 {
                bail!("no user named {name}");
            }
            eprintln!("Removed {name}; their devices stop syncing.");
            Ok(())
        }
        ["users"] => {
            let mut st = conn.prepare(
                "SELECT u.name, n.device, n.updated FROM users u LEFT JOIN now n ON n.user = u.name ORDER BY u.name",
            )?;
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?))
            })?;
            for row in rows {
                let (name, device, updated) = row?;
                match (device, updated) {
                    (Some(d), Some(u)) => println!("{name}\tlast from {d}, {}", ago(u)),
                    _ => println!("{name}\tnothing synced yet"),
                }
            }
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

fn open(path: &PathBuf) -> Result<Connection> {
    crate::config::ensure_parent(path)?;
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS users (name TEXT PRIMARY KEY, key_hash TEXT NOT NULL UNIQUE);
         CREATE TABLE IF NOT EXISTS now (user TEXT PRIMARY KEY, updated INTEGER NOT NULL, device TEXT NOT NULL, body TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS lists (user TEXT, id TEXT, name TEXT NOT NULL, source TEXT NOT NULL, pos INTEGER NOT NULL,
             deleted INTEGER NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (user, id));
         CREATE TABLE IF NOT EXISTS songs (user TEXT, list TEXT, key TEXT, track TEXT NOT NULL, pos INTEGER NOT NULL,
             present INTEGER NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (user, list, key));
         CREATE TABLE IF NOT EXISTS eps (user TEXT, key TEXT, pos INTEGER NOT NULL, played INTEGER NOT NULL, at INTEGER NOT NULL,
             PRIMARY KEY (user, key));
         CREATE TABLE IF NOT EXISTS subs (user TEXT, feed TEXT, show TEXT NOT NULL, present INTEGER NOT NULL, at INTEGER NOT NULL,
             PRIMARY KEY (user, feed));
         CREATE TABLE IF NOT EXISTS versions (user TEXT, part TEXT, version INTEGER NOT NULL, PRIMARY KEY (user, part));",
    )?;
    Ok(conn)
}

fn new_key() -> Result<String> {
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn hash(key: &str) -> String {
    sha1_smol::Sha1::from(key.trim()).digest().to_string()
}

fn ago(ms: i64) -> String {
    let s = (crate::sync::now_ms() - ms).max(0) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

fn run(conn: Connection, listen: &str) -> Result<()> {
    let server = Server::http(listen).map_err(|e| anyhow::anyhow!("listening on {listen}: {e}"))?;
    let users: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    eprintln!("music-tui sync server on http://{listen} ({users} users)");
    if users == 0 {
        eprintln!("No users yet: add one with `music-tui serve add-user <name>`.");
    }
    let conn = Arc::new(Mutex::new(conn));
    for req in server.incoming_requests() {
        let conn = conn.clone();
        std::thread::spawn(move || handle(req, &conn));
    }
    Ok(())
}

fn json() -> Header {
    Header::from_bytes("Content-Type", "application/json").unwrap()
}

fn reply(req: Request, status: u16, body: impl Into<String>) {
    let _ = req.respond(Response::from_string(body.into()).with_status_code(status).with_header(json()));
}

fn handle(mut req: Request, conn: &Mutex<Connection>) {
    let path = req.url().split('?').next().unwrap_or("").to_string();
    if path == "/v1" || path == "/v1/" {
        return reply(req, 200, format!(r#"{{"server":"music-tui","version":"{}","api":1}}"#, env!("CARGO_PKG_VERSION")));
    }
    if path != "/v1/now" && path != "/v1/sync" {
        return reply(req, 404, r#"{"error":"not found"}"#);
    }
    let key = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer ").map(str::to_string));
    let user: Option<String> = key.and_then(|k| {
        let c = conn.lock().ok()?;
        c.query_row("SELECT name FROM users WHERE key_hash = ?1", params![hash(&k)], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    });
    let Some(user) = user else {
        return reply(req, 401, r#"{"error":"unknown key"}"#);
    };
    if path == "/v1/sync" {
        if *req.method() != Method::Post {
            return reply(req, 405, r#"{"error":"POST"}"#);
        }
        let mut body = Vec::new();
        if req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
            return reply(req, 413, r#"{"error":"too large"}"#);
        }
        let Ok(r) = serde_json::from_slice::<SyncRequest>(&body) else {
            return reply(req, 400, r#"{"error":"not a sync request"}"#);
        };
        let Ok(mut c) = conn.lock() else { return reply(req, 500, "{}") };
        return match sync_state(&mut c, &user, r) {
            Ok(v) => reply(req, 200, v.to_string()),
            Err(_) => reply(req, 500, r#"{"error":"database"}"#),
        };
    }
    match req.method() {
        Method::Get => {
            let body: Option<String> = conn.lock().ok().and_then(|c| {
                c.query_row("SELECT body FROM now WHERE user = ?1", params![user], |r| r.get(0)).optional().ok().flatten()
            });
            match body {
                Some(b) => reply(req, 200, b),
                None => {
                    let _ = req.respond(Response::empty(204));
                }
            }
        }
        Method::Put => {
            let mut body = Vec::new();
            if req.as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut body).is_err() || body.len() > MAX_BODY {
                return reply(req, 413, r#"{"error":"too large"}"#);
            }
            let Ok(rec) = serde_json::from_slice::<crate::sync::Record>(&body) else {
                return reply(req, 400, r#"{"error":"not a record"}"#);
            };
            let Ok(c) = conn.lock() else { return reply(req, 500, "{}") };
            // Only a newer record replaces the stored one; either way the
            // answer is what the server now holds.
            let stored = c.execute(
                "INSERT INTO now (user, updated, device, body) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(user) DO UPDATE SET updated = excluded.updated, device = excluded.device, body = excluded.body
                 WHERE excluded.updated > now.updated",
                params![user, rec.updated, rec.device, String::from_utf8_lossy(&body)],
            );
            if stored.is_err() {
                return reply(req, 500, r#"{"error":"database"}"#);
            }
            let held: String = c
                .query_row("SELECT body FROM now WHERE user = ?1", params![user], |r| r.get(0))
                .unwrap_or_default();
            drop(c);
            reply(req, 200, held)
        }
        _ => reply(req, 405, r#"{"error":"GET or PUT"}"#),
    }
}

// --- library and podcasts ----------------------------------------------------

#[derive(serde::Deserialize)]
struct SyncRequest {
    /// The versions of each part the device last saw.
    #[serde(default)]
    versions: std::collections::HashMap<String, i64>,
    #[serde(default)]
    ops: Vec<crate::sync::Op>,
}

fn version(c: &Connection, user: &str, part: &str) -> i64 {
    c.query_row("SELECT version FROM versions WHERE user = ?1 AND part = ?2", params![user, part], |r| r.get(0))
        .unwrap_or(0)
}

/// Apply a device's changes (each wins only over an older change to the same
/// item), then answer with each part the device is behind on or changed.
fn sync_state(c: &mut Connection, user: &str, r: SyncRequest) -> Result<serde_json::Value> {
    use crate::sync::Op;
    let tx = c.transaction()?;
    let mut touched = std::collections::HashSet::new();
    for op in &r.ops {
        let (part, changed) = match op {
            Op::List { id, name, source, pos, deleted, at } => {
                let next: i64 = tx.query_row("SELECT COALESCE(MAX(pos), -1) + 1 FROM lists WHERE user = ?1", params![user], |r| r.get(0))?;
                // Seeded lists (at 1) keep the device's order; new ones go last.
                let pos = if *at <= 1 { *pos } else { next };
                let n = tx.execute(
                    "INSERT INTO lists (user, id, name, source, pos, deleted, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(user, id) DO UPDATE SET name = excluded.name, source = excluded.source,
                         deleted = excluded.deleted, at = excluded.at
                     WHERE excluded.at >= lists.at AND (lists.name != excluded.name OR lists.deleted != excluded.deleted OR lists.source != excluded.source)",
                    params![user, id, name, source, pos, *deleted as i64, at],
                )?;
                ("lib", n > 0)
            }
            Op::Song { list, key, track, pos, present, at } => {
                let was: Option<bool> = tx
                    .query_row("SELECT present FROM songs WHERE user = ?1 AND list = ?2 AND key = ?3", params![user, list, key], |r| {
                        r.get::<_, i64>(0).map(|p| p != 0)
                    })
                    .optional()?;
                let next: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(pos), -1) + 1 FROM songs WHERE user = ?1 AND list = ?2",
                    params![user, list],
                    |r| r.get(0),
                )?;
                // Seeded songs keep the device's order; a song added (or added
                // back) goes at the end, the way every app adds.
                let pos = if *at <= 1 { *pos } else { next };
                let n = tx.execute(
                    "INSERT INTO songs (user, list, key, track, pos, present, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(user, list, key) DO UPDATE SET track = excluded.track, present = excluded.present, at = excluded.at,
                         pos = CASE WHEN excluded.present AND NOT songs.present THEN ?8 ELSE songs.pos END
                     WHERE excluded.at >= songs.at AND songs.present != excluded.present",
                    params![user, list, key, track.to_string(), pos, *present as i64, at, next],
                )?;
                ("lib", n > 0 && was != Some(*present))
            }
            Op::Ep { key, pos, played, at } => {
                let n = tx.execute(
                    "INSERT INTO eps (user, key, pos, played, at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(user, key) DO UPDATE SET
                         -- Two first uploads: keep whichever got further.
                         pos = CASE WHEN excluded.at <= 1 THEN MAX(eps.pos, excluded.pos) ELSE excluded.pos END,
                         played = CASE WHEN excluded.at <= 1 THEN MAX(eps.played, excluded.played) ELSE excluded.played END,
                         at = excluded.at
                     WHERE excluded.at >= eps.at AND (eps.pos != excluded.pos OR eps.played != excluded.played)",
                    params![user, key, pos, *played as i64, at],
                )?;
                ("pods", n > 0)
            }
            Op::Sub { feed, show, present, at } => {
                let n = tx.execute(
                    "INSERT INTO subs (user, feed, show, present, at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(user, feed) DO UPDATE SET show = excluded.show, present = excluded.present, at = excluded.at
                     WHERE excluded.at >= subs.at AND subs.present != excluded.present",
                    params![user, feed, show.to_string(), *present as i64, at],
                )?;
                ("pods", n > 0)
            }
        };
        if changed && touched.insert(part) {
            tx.execute(
                "INSERT INTO versions (user, part, version) VALUES (?1, ?2, 1)
                 ON CONFLICT(user, part) DO UPDATE SET version = version + 1",
                params![user, part],
            )?;
        }
    }
    let sent: std::collections::HashSet<&str> = r
        .ops
        .iter()
        .map(|o| if matches!(o, Op::List { .. } | Op::Song { .. }) { "lib" } else { "pods" })
        .collect();
    let mut out = serde_json::Map::new();
    let mut versions = serde_json::Map::new();
    for part in ["lib", "pods"] {
        let v = version(&tx, user, part);
        versions.insert(part.into(), v.into());
        if !sent.contains(part) && r.versions.get(part) == Some(&v) {
            continue;
        }
        if part == "lib" {
            let mut st = tx.prepare("SELECT id, name, source FROM lists WHERE user = ?1 AND deleted = 0 ORDER BY pos, at")?;
            let lists: Vec<(String, String, String)> =
                st.query_map(params![user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            let mut songs = tx.prepare("SELECT track FROM songs WHERE user = ?1 AND list = ?2 AND present = 1 ORDER BY pos, at")?;
            let mut arr = Vec::new();
            for (id, name, source) in lists {
                let tracks: Vec<serde_json::Value> = songs
                    .query_map(params![user, id], |r| r.get::<_, String>(0))?
                    .filter_map(|t| t.ok().and_then(|t| serde_json::from_str(&t).ok()))
                    .collect();
                arr.push(serde_json::json!({"id": id, "name": name, "source": source, "tracks": tracks}));
            }
            out.insert("lists".into(), arr.into());
        } else {
            let mut st = tx.prepare("SELECT key, pos, played FROM eps WHERE user = ?1")?;
            let mut eps = serde_json::Map::new();
            for row in st.query_map(params![user], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
                let (k, pos, played) = row?;
                eps.insert(k, serde_json::json!({"pos": pos, "played": played != 0}));
            }
            out.insert("eps".into(), eps.into());
            let mut st = tx.prepare("SELECT show FROM subs WHERE user = ?1 AND present = 1 ORDER BY at")?;
            let subs: Vec<serde_json::Value> = st
                .query_map(params![user], |r| r.get::<_, String>(0))?
                .filter_map(|s| s.ok().and_then(|s| serde_json::from_str(&s).ok()))
                .collect();
            out.insert("subs".into(), subs.into());
        }
    }
    out.insert("versions".into(), versions.into());
    tx.commit()?;
    Ok(out.into())
}
