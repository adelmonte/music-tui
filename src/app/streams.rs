//! The Radio and Podcasts tabs. Both reuse Explore's browser (sections on
//! the left, items on the right, a stack of opened pages): each browse tab's
//! state is swapped into the `br_*` fields while it's the one in use. Also
//! here: favorite stations, subscriptions, how far into each episode you
//! got, and the live song a station is playing.

use super::*;
use crate::keys::Action;
use crate::menu::{MenuCmd, MenuItem};
use crate::podcast::{self, Show};
use crate::{radio, stream};

/// A browse tab's state while another one is in the `br_*` fields.
pub struct BrStash {
    results: Vec<deezer::View>,
    radios: Vec<deezer::View>,
    home: Vec<deezer::View>,
    stack: Vec<deezer::View>,
    sec_state: ListState,
    item_state: ListState,
    focus: PlPane,
    loading: Option<String>,
    home_requested: bool,
    query: String,
    seen: (String, Instant),
}

impl Default for BrStash {
    fn default() -> Self {
        let mut sec_state = ListState::default();
        sec_state.select(Some(0));
        let mut item_state = ListState::default();
        item_state.select(Some(0));
        Self {
            results: Vec::new(),
            radios: Vec::new(),
            home: Vec::new(),
            stack: Vec::new(),
            sec_state,
            item_state,
            focus: PlPane::List,
            loading: None,
            home_requested: false,
            query: String::new(),
            seen: (String::new(), Instant::now()),
        }
    }
}

/// An episode (a URL with a length), as opposed to a station (live).
pub fn is_episode(t: &Track) -> bool {
    stream::is_url(&t.path) && t.length_secs > 0
}

pub fn is_station(t: &Track) -> bool {
    stream::is_url(&t.path) && t.length_secs == 0
}

impl App {
    /// Put `mode`'s browse state in the `br_*` fields.
    pub(super) fn br_switch(&mut self, mode: Tab) {
        if mode == self.br_mode || !mode.browse() {
            return;
        }
        let mut s = self.br_stashes.remove(&mode).unwrap_or_default();
        std::mem::swap(&mut self.br_results, &mut s.results);
        std::mem::swap(&mut self.br_radios, &mut s.radios);
        std::mem::swap(&mut self.br_home, &mut s.home);
        std::mem::swap(&mut self.br_stack, &mut s.stack);
        std::mem::swap(&mut self.br_sec_state, &mut s.sec_state);
        std::mem::swap(&mut self.br_item_state, &mut s.item_state);
        std::mem::swap(&mut self.br_focus, &mut s.focus);
        std::mem::swap(&mut self.br_loading, &mut s.loading);
        std::mem::swap(&mut self.br_home_requested, &mut s.home_requested);
        std::mem::swap(&mut self.br_query, &mut s.query);
        std::mem::swap(&mut self.br_seen, &mut s.seen);
        self.br_stashes.insert(self.br_mode, s);
        self.br_mode = mode;
    }

    /// Run `f` with `mode`'s browse state in place, then put back the one
    /// that was.
    pub(super) fn in_mode(&mut self, mode: Tab, f: impl FnOnce(&mut Self)) {
        let was = self.br_mode;
        self.br_switch(mode);
        f(self);
        self.br_switch(was);
    }

    /// The Radio or Podcasts home page, the first time it's shown.
    pub(super) fn ensure_stream_home(&mut self) {
        let mode = self.br_mode;
        let tx = self.alt_tx.clone();
        match mode {
            Tab::Radio => {
                self.show_favorites();
                self.br_loading = Some("stations".into());
                std::thread::spawn(move || {
                    let _ = tx.send((mode, BrMsg::Home(radio::home())));
                });
            }
            Tab::Podcasts => {
                self.show_subscriptions();
                self.refresh_new_episodes();
                self.br_loading = Some("top podcasts".into());
                std::thread::spawn(move || {
                    let msg = match podcast::top() {
                        Some(v) => BrMsg::Home(vec![v]),
                        None => BrMsg::Error("Couldn't load the top podcasts.".into()),
                    };
                    let _ = tx.send((mode, msg));
                });
            }
            _ => {}
        }
    }

    pub(super) fn stream_search(&mut self, q: String) {
        let mode = self.br_mode;
        let tx = self.alt_tx.clone();
        std::thread::spawn(move || {
            let views = if mode == Tab::Radio { radio::search(&q) } else { podcast::search(&q) };
            let _ = tx.send((mode, BrMsg::Results(q, views)));
        });
    }

    /// Open a show: its episodes, newest first.
    pub(super) fn pod_open(&mut self, show: Show) {
        self.br_loading = Some(show.title.clone());
        let tx = self.alt_tx.clone();
        std::thread::spawn(move || {
            let msg = match podcast::feed(&show) {
                Ok((show, eps)) => {
                    BrMsg::Opened(deezer::View { title: show.title, items: eps.into_iter().map(|e| deezer::Item::Track(e.track)).collect() })
                }
                Err(e) => BrMsg::Error(format!("{}: {e}", show.title)),
            };
            let _ = tx.send((Tab::Podcasts, msg));
        });
    }

    fn show_favorites(&mut self) {
        if self.radio_favs.is_empty() {
            self.br_home.retain(|v| v.title != "Favorites");
        } else {
            let items = self.radio_favs.iter().cloned().map(deezer::Item::Track).collect();
            self.upsert_home(deezer::View { title: "Favorites".into(), items });
        }
    }

    fn show_subscriptions(&mut self) {
        if self.pods.subscriptions.is_empty() {
            self.br_home.retain(|v| v.title != "Subscriptions" && v.title != "New episodes");
        } else {
            let items = self.pods.subscriptions.iter().cloned().map(Show::item).collect();
            self.upsert_home(deezer::View { title: "Subscriptions".into(), items });
        }
    }

    fn refresh_new_episodes(&mut self) {
        let subs = self.pods.subscriptions.clone();
        if subs.is_empty() {
            return;
        }
        let tx = self.alt_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((Tab::Podcasts, BrMsg::ForYou(podcast::new_episodes(&subs))));
        });
    }

    /// `l` in Radio: add or remove the highlighted (or playing) station.
    pub(super) fn radio_toggle_favorite(&mut self) {
        let t = self.br_target_track().filter(is_station).or_else(|| self.playing_track().filter(is_station));
        let Some(t) = t else { return self.notify("Highlight a station first.") };
        if let Some(i) = self.radio_favs.iter().position(|f| f.path == t.path) {
            self.radio_favs.remove(i);
            self.notify(format!("Removed '{}' from favorites.", t.title));
        } else {
            self.radio_favs.insert(0, t.clone());
            self.notify(format!("Added '{}' to favorites.", t.title));
        }
        radio::save_favorites(&self.radio_favs);
        self.in_mode(Tab::Radio, |app| app.show_favorites());
    }

    /// `l` in Podcasts: subscribe to the highlighted show, or mark the
    /// highlighted episode played or unplayed.
    pub(super) fn pod_toggle(&mut self) {
        let item = self.br_current().and_then(|v| self.br_item_state.selected().and_then(|i| v.items.get(i)).cloned());
        match item {
            Some(deezer::Item::Show(show)) if self.br_focus == PlPane::Tracks => self.toggle_subscription(show),
            Some(deezer::Item::Track(t)) if self.br_focus == PlPane::Tracks => self.toggle_played(&t),
            _ => match self.queue_selected_track().filter(|_| self.queue_focused()).filter(is_episode) {
                Some(t) => self.toggle_played(&t),
                None => self.notify("Highlight a show or an episode first."),
            },
        }
    }

    pub(super) fn toggle_subscription(&mut self, show: Show) {
        if let Some(i) = self.pods.subscriptions.iter().position(|s| s.feed == show.feed) {
            self.pods.subscriptions.remove(i);
            self.notify(format!("Unsubscribed from '{}'.", show.title));
        } else {
            self.notify(format!("Subscribed to '{}'.", show.title));
            self.pods.subscriptions.push(show);
            self.pods.subscriptions.sort_by_key(|s| s.title.to_lowercase());
        }
        self.pods_dirty = true;
        self.save_pods();
        self.in_mode(Tab::Podcasts, |app| {
            app.show_subscriptions();
            app.refresh_new_episodes();
        });
    }

    pub(super) fn toggle_played(&mut self, t: &Track) {
        let url = t.path.to_string_lossy().into_owned();
        if self.pods.played.remove(&url) {
            self.notify(format!("'{}' marked unplayed.", t.title));
        } else {
            self.pods.played.insert(url.clone());
            self.pods.progress.remove(&url);
            self.notify(format!("'{}' marked played.", t.title));
        }
        self.pods_dirty = true;
        self.save_pods();
    }

    pub(super) fn save_pods(&mut self) {
        if std::mem::take(&mut self.pods_dirty) {
            self.pods.save();
        }
    }

    pub fn playing_track(&self) -> Option<Track> {
        let t = self.now.and_then(|i| self.queue.get(i))?;
        (self.audio.current_path() == Some(t.path.as_path())).then(|| t.clone())
    }

    /// Record how far into the playing episode we are; near the end counts
    /// as finished.
    pub(super) fn note_progress(&mut self) {
        let Some(t) = self.playing_track().filter(is_episode) else { return };
        let len = self.audio.duration().as_secs();
        let pos = self.audio.position().as_secs();
        let url = t.path.to_string_lossy().into_owned();
        if len > 60 && (pos + 30 >= len || pos * 100 >= len * 95) {
            if self.pods.played.insert(url.clone()) | self.pods.progress.remove(&url).is_some() {
                self.pods_dirty = true;
            }
        } else if pos > 5 && self.pods.progress.get(&url) != Some(&pos) {
            self.pods.progress.insert(url, pos);
            self.pods_dirty = true;
        }
    }

    /// Seconds left in an episode started earlier, for its row.
    pub fn episode_left(&self, t: &Track) -> Option<u64> {
        let p = *self.pods.progress.get(t.path.to_str()?)?;
        Some(t.length_secs.saturating_sub(p))
    }

    pub fn episode_played(&self, t: &Track) -> bool {
        t.path.to_str().is_some_and(|u| self.pods.played.contains(u))
    }

    /// A station or episode just started: resume the episode where it was
    /// left, and count the station's play.
    pub(super) fn on_track_started(&mut self, t: &Track) {
        self.stream_title_seen = None;
        if is_station(t) {
            radio::count_play(&t.path);
        } else if is_episode(t)
            && let Some(&p) = t.path.to_str().and_then(|u| self.pods.progress.get(u))
            && p > 10
            && p + 30 < t.length_secs
        {
            self.audio.seek_to(Duration::from_secs(p));
            self.notify(format!("Resuming at {}.", fmt_hms(p)));
        }
    }

    /// Per tick: show the station's live song as it changes.
    pub(super) fn tick_streams(&mut self) {
        let title = self.audio.stream_title();
        if title == self.stream_title_seen {
            return;
        }
        self.stream_title_seen = title.clone();
        if let (Some(song), Some(station)) = (title, self.playing_track().filter(is_station)) {
            self.now_playing = format!("▶ {song}  ·  {}", station.title);
            if self.notify_until.is_none() {
                self.status = self.now_playing.clone();
            }
        }
    }

    /// `[` / `]`: 10 s back and 30 s ahead in an episode, 5 s otherwise.
    pub(super) fn seek_step(&self, forward: bool) -> i64 {
        match (self.playing_track().is_some_and(|t| is_episode(&t)), forward) {
            (true, true) => 30,
            (true, false) => -10,
            (false, true) => 5,
            (false, false) => -5,
        }
    }

    /// Look up the song a station is playing in Explore.
    pub(super) fn find_in_explore(&mut self, song: String) {
        if !self.config.browser.enabled {
            return self.notify("Turn on the Explore tab in Settings to look songs up.");
        }
        self.set_tab(Tab::Browser);
        self.search = song;
        self.search_cursor = self.search.chars().count();
        self.br_search_now();
    }

    /// The right-click menu in the Radio and Podcasts tabs.
    pub(super) fn stream_menu_items(&self) -> (Option<String>, Vec<MenuItem>) {
        let k = |a: Action| self.keymap.label(a);
        let act = |label: &str, a: Action| MenuItem::new(label, k(a), MenuCmd::Act(a));
        let Some(view) = self.br_current() else { return (None, Vec::new()) };
        if self.br_focus == PlPane::List {
            return (Some(view.title.clone()), vec![act("Add all to queue", Action::Queue)]);
        }
        let item = self.br_item_state.selected().and_then(|i| view.items.get(i));
        let mut v = Vec::new();
        let title = match item {
            Some(deezer::Item::Track(t)) => {
                v.push(MenuItem::new("Play", k(Action::Activate), MenuCmd::Play));
                v.push(act("Play next", Action::PlayNext));
                v.push(act("Add to queue", Action::Queue));
                v.push(MenuItem::sep());
                if is_station(t) {
                    let fav = self.radio_favs.iter().any(|f| f.path == t.path);
                    v.push(act(if fav { "Remove from favorites" } else { "Add to favorites" }, Action::Like));
                    if let Some(song) = self.live_song_of(t) {
                        v.push(MenuItem::new(format!("Find “{song}” in Explore"), String::new(), MenuCmd::FindSong(song)));
                    }
                } else {
                    v.push(act(if self.episode_played(t) { "Mark unplayed" } else { "Mark played" }, Action::Like));
                    if let Some(show) = podcast::show_of(&t.path).filter(|s| self.br_stack.last().is_none_or(|v| v.title != s.title)) {
                        v.push(MenuItem::new(format!("Go to {}", show.title), String::new(), MenuCmd::OpenShow(show)));
                    }
                }
                t.title.clone()
            }
            Some(deezer::Item::Show(show)) => {
                v.push(MenuItem::new("Open", k(Action::Activate), MenuCmd::Play));
                let subbed = self.pods.subscribed(&show.feed);
                v.push(act(if subbed { "Unsubscribe" } else { "Subscribe" }, Action::Like));
                show.title.clone()
            }
            _ => view.title.clone(),
        };
        // On a show's page: subscribe to the show itself.
        if self.tab == Tab::Podcasts
            && let Some(show) = self.open_show()
        {
            let subbed = self.pods.subscribed(&show.feed);
            v.push(MenuItem::sep());
            v.push(MenuItem::new(if subbed { "Unsubscribe from this show" } else { "Subscribe to this show" }, String::new(), MenuCmd::Subscribe(show)));
        }
        if !self.br_stack.is_empty() {
            v.push(act("Back", Action::Back));
        }
        (Some(title), v)
    }

    /// The show whose page is open, if any.
    pub fn open_show(&self) -> Option<Show> {
        let page = self.br_stack.last()?;
        page.items.iter().find_map(|i| match i {
            deezer::Item::Track(t) => podcast::show_of(&t.path),
            _ => None,
        })
    }

    /// The live song of a station, if it's the one playing.
    pub fn live_song_of(&self, t: &Track) -> Option<String> {
        (self.audio.current_path() == Some(t.path.as_path())).then(|| self.audio.stream_title()).flatten()
    }
}

/// `1:02:03` / `12:34`.
pub fn fmt_hms(s: u64) -> String {
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

