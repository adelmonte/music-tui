//! `music-tui serve`: the sync server. It keeps one resume record per user
//! (see `sync`) in a small SQLite file and hands it to that user's devices.
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
            conn.execute("DELETE FROM now WHERE user = ?1", params![name])?;
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
         CREATE TABLE IF NOT EXISTS now (user TEXT PRIMARY KEY, updated INTEGER NOT NULL, device TEXT NOT NULL, body TEXT NOT NULL);",
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
    if path != "/v1/now" {
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
