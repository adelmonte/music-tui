//! Sync with the other devices (see `crate::sync`). Resume: push the queue
//! and position when playback pauses, changes song, or every half minute; at
//! startup, take the server's record if it's newer and nothing has been
//! played here yet. Library and podcasts: send what changed since the last
//! sync (checked every half minute, and every two minutes regardless) and
//! take the merged result.

use std::hash::{Hash, Hasher};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::App;
use crate::model::Track;
use crate::playlist::Playlist;
use crate::spotify;
use crate::sync::{self, Answer, Base, Record, State};

/// What was last pushed, to tell when there's something new.
#[derive(Default, PartialEq)]
pub(super) struct Pushed {
    queue: u64,
    now: Option<usize>,
    playing: bool,
    position: f64,
}

pub(super) struct SyncState {
    /// When this device's playback last changed (or a pulled record's time).
    pub updated: i64,
    last: Pushed,
    /// The queue as it was at startup: a pulled record only replaces it while
    /// nothing has been played or queued since.
    start_queue: u64,
    pushed_at: Instant,
    checked_at: Instant,
    pull: Option<mpsc::Receiver<Record>>,
    lib: LibSync,
}

/// The library and podcasts part.
#[derive(Default)]
struct LibSync {
    /// What the server last confirmed (None before the first sync).
    base: Option<Base>,
    loaded: bool,
    /// The local state's hash when last in step with the server.
    synced: u64,
    checked_at: Option<Instant>,
    synced_at: Option<Instant>,
    /// A sync on its way: the state's hash when sent, and the answer.
    inflight: Option<(u64, State, mpsc::Receiver<anyhow::Result<Answer>>)>,
}

fn base_path() -> std::path::PathBuf {
    crate::config::session_path().with_file_name("sync_base.json")
}

fn state_hash(s: &State) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_vec(s).unwrap_or_default().hash(&mut h);
    h.finish()
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            updated: 0,
            last: Pushed::default(),
            start_queue: 0,
            pushed_at: Instant::now(),
            checked_at: Instant::now(),
            pull: None,
            lib: LibSync::default(),
        }
    }
}

impl App {
    fn queue_hash(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for t in &self.queue {
            t.path.hash(&mut h);
        }
        h.finish()
    }

    fn sync_snapshot(&self) -> Pushed {
        Pushed {
            queue: self.queue_hash(),
            now: self.now,
            playing: self.audio.is_playing(),
            position: self.position_secs(),
        }
    }

    /// After the session is restored: note where things stand and ask the
    /// server for its record in the background.
    pub(super) fn sync_start(&mut self) {
        self.sync.last = self.sync_snapshot();
        self.sync.start_queue = self.sync.last.queue;
        if !self.config.sync.enabled() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let cfg = self.config.sync.clone();
        std::thread::spawn(move || {
            if let Ok(Some(rec)) = sync::pull(&cfg) {
                let _ = tx.send(rec);
            }
        });
        self.sync.pull = Some(rx);
    }

    /// Called every tick; does its work about once a second.
    pub(super) fn sync_tick(&mut self) {
        if !self.config.sync.enabled() || self.sync.checked_at.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.sync.checked_at = Instant::now();
        if let Some(rx) = &self.sync.pull
            && let Ok(rec) = rx.try_recv()
        {
            self.sync.pull = None;
            let untouched = self.queue_hash() == self.sync.start_queue && !self.audio.is_playing();
            if rec.updated > self.sync.updated && untouched {
                self.apply_record(rec);
            }
        }
        let snap = self.sync_snapshot();
        let last = &self.sync.last;
        let changed = snap.queue != last.queue
            || snap.now != last.now
            || snap.playing != last.playing
            || (!snap.playing && (snap.position - last.position).abs() > 2.0)
            || (snap.playing && self.sync.pushed_at.elapsed() >= Duration::from_secs(30));
        if changed {
            self.sync_push(snap, None);
        }
        self.lib_tick();
    }

    /// This device's library and podcasts, as the server shapes them.
    fn lib_state(&self) -> State {
        let base = self.sync.lib.base.clone().unwrap_or_default();
        let lists = if self.config.spotify.enabled {
            self.sp_store
                .iter()
                .map(|p| {
                    let id = base.ids.get(&p.name).cloned().unwrap_or_else(|| sync::list_id(&p.name));
                    let source = if id == "liked" { "liked" } else if id.starts_with('s') { "spotify" } else { "mine" };
                    sync::List {
                        id,
                        name: p.name.clone(),
                        source: source.into(),
                        tracks: p.tracks.iter().filter_map(|t| serde_json::to_value(sync::to_wire(t)).ok()).collect(),
                    }
                })
                .collect()
        } else {
            // The Streaming tab is off: leave the library as the server has it.
            base.lists
                .iter()
                .map(|(id, name, keys)| sync::List {
                    id: id.clone(),
                    name: name.clone(),
                    source: String::new(),
                    tracks: keys.iter().map(|k| serde_json::json!({"key": k})).collect(),
                })
                .collect()
        };
        let mut eps = std::collections::BTreeMap::new();
        for (url, &pos) in &self.pods.progress {
            eps.insert(format!("ep:{url}"), sync::Ep { pos: pos as i64, played: false });
        }
        for url in &self.pods.played {
            eps.insert(format!("ep:{url}"), sync::Ep { pos: 0, played: true });
        }
        let subs = self.pods.subscriptions.iter().filter_map(|s| serde_json::to_value(s).ok()).collect();
        State { lists, eps, subs }
    }

    fn lib_tick(&mut self) {
        if !self.sync.lib.loaded {
            self.sync.lib.loaded = true;
            self.sync.lib.base = std::fs::read(base_path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
        }
        if let Some((hash, sent, rx)) = &self.sync.lib.inflight {
            let Ok(result) = rx.try_recv() else { return };
            let (hash, sent) = (*hash, sent.clone());
            self.sync.lib.inflight = None;
            match result {
                // Changed meanwhile: send again with that change first.
                Ok(_) if state_hash(&self.lib_state()) != hash => self.sync.lib.synced = 0,
                Ok(answer) => self.lib_apply(sent, answer),
                Err(_) => self.sync.lib.synced_at = Some(Instant::now()),
            }
            return;
        }
        let due = self.sync.lib.checked_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
        if !due {
            return;
        }
        self.sync.lib.checked_at = Some(Instant::now());
        let cur = self.lib_state();
        let hash = state_hash(&cur);
        let stale = self.sync.lib.synced_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(120));
        if hash == self.sync.lib.synced && !stale {
            return;
        }
        self.lib_send(cur, hash, None);
    }

    fn lib_send(&mut self, cur: State, hash: u64, wait: Option<Duration>) {
        let ops = sync::diff(self.sync.lib.base.as_ref(), &cur, sync::now_ms());
        let versions = self.sync.lib.base.as_ref().map(|b| b.versions.clone()).unwrap_or_default();
        let cfg = self.config.sync.clone();
        let (tx, rx) = mpsc::channel();
        let timeout = wait.unwrap_or(Duration::from_secs(60));
        let job = move || {
            let _ = tx.send(sync::sync_state(&cfg, &versions, &ops, timeout));
        };
        match wait {
            Some(_) => job(),
            None => {
                std::thread::spawn(job);
            }
        }
        self.sync.lib.inflight = Some((hash, cur, rx));
    }

    /// Take the server's merged library and podcasts.
    fn lib_apply(&mut self, sent: State, answer: Answer) {
        let mut merged = sent;
        if let Some(lists) = &answer.lists
            && self.config.spotify.enabled
        {
            let old: std::collections::HashMap<std::path::PathBuf, Track> =
                self.sp_store.iter().flat_map(|p| p.tracks.iter()).map(|t| (t.path.clone(), t.clone())).collect();
            let kept: std::collections::HashSet<String> =
                lists.iter().map(|l| if l.id == "liked" { spotify::LIKED.to_string() } else { l.name.clone() }).collect();
            let mut deleted = spotify::deleted_lists();
            for p in &self.sp_store {
                if !kept.contains(&p.name) && !deleted.contains(&p.name) {
                    deleted.push(p.name.clone());
                }
            }
            spotify::set_deleted_lists(&deleted);
            self.sp_store = lists
                .iter()
                .map(|l| Playlist {
                    name: if l.id == "liked" { spotify::LIKED.to_string() } else { l.name.clone() },
                    tracks: l
                        .tracks
                        .iter()
                        .filter_map(|v| serde_json::from_value::<sync::Wire>(v.clone()).ok())
                        .filter_map(|w| sync::from_wire_streamed(&w))
                        .enumerate()
                        .map(|(i, t)| {
                            let mut t = old.get(&t.path).cloned().unwrap_or(t);
                            t.id = i as i64;
                            t
                        })
                        .collect(),
                })
                .collect();
            spotify::save_store(&self.sp_store);
            self.rebuild_streaming();
            merged.lists = lists.clone();
            for l in &mut merged.lists {
                if l.id == "liked" {
                    l.name = spotify::LIKED.into();
                }
            }
        }
        if let (Some(eps), Some(subs)) = (&answer.eps, &answer.subs) {
            self.pods.progress.clear();
            self.pods.played.clear();
            for (k, e) in eps {
                let Some(url) = k.strip_prefix("ep:") else { continue };
                if e.played {
                    self.pods.played.insert(url.to_string());
                } else if e.pos > 0 {
                    self.pods.progress.insert(url.to_string(), e.pos as u64);
                }
            }
            let mut shows: Vec<crate::podcast::Show> = subs.iter().filter_map(|s| serde_json::from_value(s.clone()).ok()).collect();
            shows.sort_by_key(|s| s.title.to_lowercase());
            self.pods.subscriptions = shows;
            self.pods.save();
            merged.eps = eps.clone();
            merged.subs = subs.clone();
        }
        let mut base = self.sync.lib.base.take().unwrap_or_default();
        base.take(&merged, &answer);
        if let Ok(body) = serde_json::to_vec(&base) {
            let _ = crate::config::ensure_parent(&base_path());
            let _ = std::fs::write(base_path(), body);
        }
        self.sync.lib.base = Some(base);
        self.sync.lib.synced = state_hash(&self.lib_state());
        self.sync.lib.synced_at = Some(Instant::now());
    }

    fn sync_push(&mut self, snap: Pushed, wait: Option<Duration>) {
        let Some(i) = self.now.filter(|&i| i < self.queue.len()) else {
            self.sync.last = snap;
            return;
        };
        let device = sync::device_name(&self.config.sync);
        let rec = sync::record(&self.queue, i, snap.position, &device);
        self.sync.updated = rec.updated;
        self.sync.last = snap;
        self.sync.pushed_at = Instant::now();
        let cfg = self.config.sync.clone();
        match wait {
            Some(t) => {
                let _ = sync::push(&cfg, &rec, t);
            }
            None => {
                std::thread::spawn(move || {
                    let _ = sync::push(&cfg, &rec, Duration::from_secs(15));
                });
            }
        }
    }

    /// On quit: send the final position (waiting briefly for the server).
    pub fn sync_on_quit(&mut self) {
        if !self.config.sync.enabled() {
            return;
        }
        let snap = self.sync_snapshot();
        let last = &self.sync.last;
        if snap.playing || snap.queue != last.queue || snap.now != last.now || (snap.position - last.position).abs() > 2.0 {
            let snap = Pushed { playing: false, ..snap };
            self.sync_push(snap, Some(Duration::from_secs(3)));
            self.save_session();
        }
        // Library or podcast changes not yet sent.
        if self.sync.lib.inflight.is_none() && self.sync.lib.loaded {
            let cur = self.lib_state();
            let hash = state_hash(&cur);
            if hash != self.sync.lib.synced {
                self.lib_send(cur, hash, Some(Duration::from_secs(5)));
                self.lib_tick();
            }
        }
    }

    /// Take over another device's queue, loaded paused at its position.
    fn apply_record(&mut self, rec: Record) {
        let mut queue = Vec::new();
        let mut index = 0;
        for (i, w) in rec.queue.iter().enumerate() {
            if let Some(t) = sync::from_wire(w, &self.library) {
                if i == rec.index {
                    index = queue.len();
                }
                queue.push(t);
            } else if i < rec.index {
                index = queue.len();
            }
        }
        if queue.is_empty() {
            return;
        }
        self.sync.updated = rec.updated;
        self.queue = queue;
        self.queue_state.select(Some(index));
        self.load_paused(index, rec.position_ms as f64 / 1000.0);
        if !self.now_playing.is_empty() {
            self.now_playing = format!("{}  · from {}", self.now_playing, rec.device);
            self.status = self.now_playing.clone();
        }
        self.sync.last = self.sync_snapshot();
        self.sync.start_queue = self.sync.last.queue;
    }
}
