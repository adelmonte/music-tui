//! Resume sync with the other devices (see `crate::sync`): push the queue and
//! position when playback pauses, changes song, or every half minute; at
//! startup, take the server's record if it's newer and nothing has been
//! played here yet.

use std::hash::{Hash, Hasher};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::App;
use crate::sync::{self, Record};

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
