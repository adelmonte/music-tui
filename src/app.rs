use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use ratatui_image::protocol::StatefulProtocol;

use crate::art::Art;
use crate::audio::Audio;
use crate::config::{Config, Theme};
use crate::db;
use crate::model::{AlbumRef, Library, RepeatMode, Track};
use crate::playlist::{self, Playlist};
use crate::scan::{self, ScanMsg};

/// Everything Tab can land on. `Search` is the text field; the rest are the
/// browseable lists. Mouse hit-testing and column movement only concern the
/// four list variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Search,
    Artists,
    Albums,
    Tracks,
    Queue,
    Seekbar,
    Transport,
    Volume,
}

/// Full Tab cycle, top to bottom.
const TAB_ORDER: [Focus; 8] = [
    Focus::Search,
    Focus::Artists,
    Focus::Albums,
    Focus::Tracks,
    Focus::Queue,
    Focus::Seekbar,
    Focus::Transport,
    Focus::Volume,
];

impl Focus {
    fn is_list(self) -> bool {
        matches!(
            self,
            Focus::Artists | Focus::Albums | Focus::Tracks | Focus::Queue
        )
    }
    /// Horizontal column movement (←/→, h/l) among the browser columns only.
    fn left(self) -> Focus {
        match self {
            Focus::Albums => Focus::Artists,
            Focus::Tracks => Focus::Albums,
            Focus::Queue => Focus::Tracks,
            other => other,
        }
    }
    fn right(self) -> Focus {
        match self {
            Focus::Artists => Focus::Albums,
            Focus::Albums => Focus::Tracks,
            Focus::Tracks => Focus::Queue,
            other => other,
        }
    }
    fn next(self) -> Focus {
        let i = TAB_ORDER.iter().position(|&f| f == self).unwrap_or(0);
        TAB_ORDER[(i + 1) % TAB_ORDER.len()]
    }
    fn prev(self) -> Focus {
        let i = TAB_ORDER.iter().position(|&f| f == self).unwrap_or(0);
        TAB_ORDER[(i + TAB_ORDER.len() - 1) % TAB_ORDER.len()]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Browser,
    Playlists,
    Settings,
}

/// Which pane is active within the Playlists tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlPane {
    List,
    Tracks,
}

/// What a left-button drag is currently manipulating, set on mouse-down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drag {
    Seek,
    Volume,
    /// Resizing the boundary between browser column `i` and `i + 1`.
    ColDivider(usize),
    /// Resizing the tracks/queue split in the third column.
    QueueDivider,
}

/// A decoded cover image returned from the background art worker, tagged with
/// the track path it belongs to so stale results can be discarded.
struct ArtMsg {
    path: PathBuf,
    image: Option<DynamicImage>,
}

/// Layout rectangles recorded each frame so the input layer can hit-test mouse
/// clicks. The list rects are the *inner* content areas (inside borders).
#[derive(Default, Clone)]
pub struct Rects {
    pub search: Rect,
    pub artists: Rect,
    pub albums: Rect,
    pub tracks: Rect,
    pub queue: Rect,
    /// The body region containing the three columns (for divider hit-testing).
    pub body: Rect,
    /// X positions of the two draggable column dividers.
    pub col_div: [u16; 2],
    /// The third column's area and the y of the tracks/queue divider.
    pub col3: Rect,
    pub queue_div: u16,
    pub seekbar: Rect,
    pub volume: Rect,
    /// The bottom-left album-art region (click to open the art popup).
    pub art: Rect,
    /// The "? help" hint in the top-right (click to open help).
    pub help_hint: Rect,
    pub tab_browser: Rect,
    pub tab_playlists: Rect,
    pub tab_settings: Rect,
    pub pl_list: Rect,
    pub pl_tracks: Rect,
    pub btn_shuffle: Rect,
    pub btn_prev: Rect,
    pub btn_play: Rect,
    pub btn_next: Rect,
    pub btn_repeat: Rect,
}

pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub library: Library,
    pub audio: Audio,
    pub art: Art,

    pub tab: Tab,
    pub dragging: Option<Drag>,
    pub focus: Focus,
    /// Which transport button is selected when the Transport row is focused.
    pub transport_sel: usize,
    /// Browser column width percentages [left, middle, right], summing to 100.
    pub col_widths: [u16; 3],
    /// Render transport icons at 2x via the kitty text-sizing protocol.
    pub big_icons: bool,
    /// Queue panel height percentage within the third column.
    pub queue_pct: u16,
    /// Whether all-library art pre-caching is enabled.
    pub cache_all_art: bool,
    pub search: String,
    /// Cursor position within `search`, as a character index (0..=char count).
    pub search_cursor: usize,
    /// Last text removed by a kill command (Ctrl-W/U/K/Alt-D), for Ctrl-Y yank.
    kill_buffer: String,

    // Playlists tab state.
    pub playlists: Vec<Playlist>,
    pub playlist_state: ListState,
    pub pl_track_state: ListState,
    pub pl_focus: PlPane,
    /// Some(buffer) while naming a new playlist.
    pub editing_playlist_name: Option<String>,

    pub artist_state: ListState,
    pub album_state: ListState,
    pub track_state: ListState,
    pub queue_state: ListState,

    pub view_artists: Vec<String>,
    pub view_albums: Vec<AlbumRef>,
    pub view_tracks: Vec<Track>,

    /// The active play order. The Queue panel renders this list.
    pub queue: Vec<Track>,
    pub now: Option<usize>,
    pub shuffle: bool,
    pub repeat: RepeatMode,

    pub show_help: bool,
    pub status: String,
    /// The persistent now-playing line shown when no transient message is active.
    now_playing: String,
    /// While set and unexpired, `status` holds a transient message; once it
    /// elapses the next tick restores `now_playing`.
    notify_until: Option<Instant>,
    pub scanning: Option<(usize, usize)>,
    /// Visual scrubber position during a seekbar drag.
    pub pending_seek: Option<f64>,
    /// When the last throttled seek fired, so we don't flood rodio.
    last_seek_at: Option<std::time::Instant>,
    /// Time + cell of the last left click, for double-click detection.
    last_click: Option<(Instant, u16, u16)>,
    /// Some(buffer) when editing the music directory path in the Library tab.
    pub editing_path: Option<String>,

    pub should_quit: bool,

    pub rects: Rects,
    pub art_proto: Option<StatefulProtocol>,
    /// The current track's decoded cover, kept so the art can be re-encoded on a
    /// font-size change and used to build the popup protocol.
    art_image: Option<DynamicImage>,
    /// Large centered album-art overlay (toggled with `i`).
    pub show_art_popup: bool,
    pub art_popup_proto: Option<StatefulProtocol>,
    art_path: Option<PathBuf>,
    art_tx: Sender<ArtMsg>,
    art_rx: Receiver<ArtMsg>,
    /// Decoded, downscaled covers keyed by track path, so revisiting a track
    /// rebuilds its art instantly instead of re-decoding.
    art_cache: HashMap<PathBuf, DynamicImage>,

    rng: u64,
    scan_rx: Option<Receiver<ScanMsg>>,
}

impl App {
    pub fn new(config: Config, library: Library, audio: Audio, art: Art) -> Self {
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15)
            | 1;
        let theme = config.theme.resolve();
        let col_widths = config.validated_columns();
        let queue_pct = config.validated_queue_pct();
        let big_icons = config.ui.big_transport_icons;
        let cache_all_art = config.ui.cache_all_art;
        let playlists = playlist::load();
        let (art_tx, art_rx) = channel::<ArtMsg>();
        let mut app = App {
            config,
            theme,
            library,
            audio,
            art,
            tab: Tab::Browser,
            dragging: None,
            focus: Focus::Artists,
            transport_sel: 2,
            col_widths,
            big_icons,
            queue_pct,
            cache_all_art,
            search: String::new(),
            search_cursor: 0,
            kill_buffer: String::new(),
            playlists,
            playlist_state: ListState::default(),
            pl_track_state: ListState::default(),
            pl_focus: PlPane::List,
            editing_playlist_name: None,
            artist_state: ListState::default(),
            album_state: ListState::default(),
            track_state: ListState::default(),
            queue_state: ListState::default(),
            view_artists: Vec::new(),
            view_albums: Vec::new(),
            view_tracks: Vec::new(),
            queue: Vec::new(),
            now: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            show_help: false,
            status: String::new(),
            now_playing: String::new(),
            notify_until: None,
            scanning: None,
            pending_seek: None,
            last_seek_at: None,
            last_click: None,
            editing_path: None,
            should_quit: false,
            rects: Rects::default(),
            art_proto: None,
            art_image: None,
            show_art_popup: false,
            art_popup_proto: None,
            art_path: None,
            art_tx,
            art_rx,
            art_cache: HashMap::new(),
            rng: seed,
            scan_rx: None,
        };
        app.refresh_views();
        app.clamp_pl_states();
        if app.library.is_empty() {
            app.status = "Library empty — press 3 for settings, then r to scan.".into();
        } else {
            app.status = format!("{} tracks loaded.", app.library.tracks.len());
        }
        if app.cache_all_art {
            app.start_art_precache();
        }
        app
    }

    fn next_rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    // --- view derivation -------------------------------------------------

    pub fn selected_artist(&self) -> Option<String> {
        let idx = self.artist_state.selected().unwrap_or(0);
        if idx == 0 {
            None
        } else {
            self.view_artists.get(idx).cloned()
        }
    }

    pub fn selected_album(&self) -> Option<AlbumRef> {
        let idx = self.album_state.selected().unwrap_or(0);
        if idx == 0 {
            None
        } else {
            self.view_albums.get(idx).cloned()
        }
    }

    /// Recompute the three column views from the current search + selection,
    /// clamping selections to the new lengths.
    pub fn refresh_views(&mut self) {
        let q = self.search.to_lowercase();
        let mut artists = self.library.album_artists(&q, self.config.ui.sort_articles);
        artists.insert(0, "All".to_string());
        self.view_artists = artists;
        clamp(&mut self.artist_state, self.view_artists.len());

        let artist = self.selected_artist();
        let mut albums = self.library.albums(&q, artist.as_deref());
        albums.insert(0, AlbumRef { album_artist: String::new(), title: "All".to_string(), year: None });
        self.view_albums = albums;
        clamp(&mut self.album_state, self.view_albums.len());

        let album = self.selected_album();
        self.view_tracks = self
            .library
            .tracks_view(&q, artist.as_deref(), album.as_ref());
        clamp(&mut self.track_state, self.view_tracks.len());
    }

    // --- navigation ------------------------------------------------------

    pub fn is_searching(&self) -> bool {
        self.focus == Focus::Search
    }

    pub fn move_selection(&mut self, delta: i32) {
        let (state, len) = match self.focus {
            Focus::Artists => (&mut self.artist_state, self.view_artists.len()),
            Focus::Albums => (&mut self.album_state, self.view_albums.len()),
            Focus::Tracks => (&mut self.track_state, self.view_tracks.len()),
            Focus::Queue => (&mut self.queue_state, self.queue.len()),
            _ => return,
        };
        move_state(state, len, delta);
        // Moving the artist or album highlight re-derives the columns to the right.
        if matches!(self.focus, Focus::Artists | Focus::Albums) {
            self.refresh_views();
        }
    }

    pub fn move_column(&mut self, right: bool) {
        self.focus = if right {
            self.focus.right()
        } else {
            self.focus.left()
        };
    }

    /// Arrow / hjkl handlers that mean different things per focus: list nav on
    /// the columns, seek on the progress bar, button select on the transport,
    /// volume on the slider.
    pub fn on_left(&mut self) {
        match self.focus {
            f if f.is_list() => self.move_column(false),
            Focus::Seekbar => self.seek_relative(-5),
            Focus::Transport => self.transport_step(-1),
            Focus::Volume => self.volume_down(),
            _ => {}
        }
    }
    pub fn on_right(&mut self) {
        match self.focus {
            f if f.is_list() => self.move_column(true),
            Focus::Seekbar => self.seek_relative(5),
            Focus::Transport => self.transport_step(1),
            Focus::Volume => self.volume_up(),
            _ => {}
        }
    }
    pub fn on_up(&mut self) {
        match self.focus {
            Focus::Volume => self.volume_up(),
            _ => self.move_selection(-1),
        }
    }
    pub fn on_down(&mut self) {
        match self.focus {
            Focus::Volume => self.volume_down(),
            _ => self.move_selection(1),
        }
    }

    // --- column resizing -------------------------------------------------

    const MIN_COL: i32 = 10;

    /// Keyboard resize (Alt+] wider / Alt+[ narrower) of the focused column,
    /// borrowing width from a neighbor. Persists the new widths.
    pub fn resize_focused(&mut self, wider: bool) {
        // The queue resizes vertically (its share of the third column).
        if self.focus == Focus::Queue {
            let d = if wider { 4 } else { -4 };
            let v = (self.queue_pct as i32 + d).clamp(10, 60) as u16;
            if v != self.queue_pct {
                self.queue_pct = v;
                self.config.layout.queue_pct = v;
                let _ = self.config.save();
            }
            return;
        }
        let idx = match self.focus {
            Focus::Artists => 0,
            Focus::Albums => 1,
            Focus::Tracks => 2,
            _ => return,
        };
        // Grow by stealing from the right neighbor, except the rightmost steals left.
        let donor = if idx == 2 { 1 } else { idx + 1 };
        let step = 4;
        let mut cols = self.col_widths.map(|v| v as i32);
        let d = if wider { step } else { -step };
        if cols[idx] + d >= Self::MIN_COL && cols[donor] - d >= Self::MIN_COL {
            cols[idx] += d;
            cols[donor] -= d;
            self.col_widths = cols.map(|v| v as u16);
            self.persist_columns();
        }
    }

    /// Set a divider boundary from a mouse-x position (no save; that happens on
    /// release to avoid a write per drag event).
    pub fn drag_divider(&mut self, divider: usize, x: u16) {
        let body = self.rects.body;
        if body.width == 0 {
            return;
        }
        let rel = (x as i32 - body.x as i32).clamp(0, body.width as i32) as f64;
        let pct = (rel / body.width as f64 * 100.0).round() as i32;
        let mut cols = self.col_widths.map(|v| v as i32);
        match divider {
            0 => {
                let pair = cols[0] + cols[1];
                cols[0] = pct.clamp(Self::MIN_COL, pair - Self::MIN_COL);
                cols[1] = pair - cols[0];
            }
            _ => {
                let cum = pct.clamp(cols[0] + Self::MIN_COL, 100 - Self::MIN_COL);
                cols[1] = cum - cols[0];
                cols[2] = 100 - cum;
            }
        }
        self.col_widths = cols.map(|v| v as u16);
    }

    /// Persist the current column widths to the config file.
    pub fn persist_columns(&mut self) {
        self.config.layout.columns = self.col_widths.to_vec();
        let _ = self.config.save();
    }

    /// Drag the tracks/queue divider: set the queue height from a mouse y within
    /// the third column (no save; that happens on release).
    pub fn drag_queue_divider(&mut self, y: u16) {
        let c = self.rects.col3;
        if c.height == 0 {
            return;
        }
        let bottom = c.y + c.height;
        let qh = (bottom as i32 - y as i32).clamp(0, c.height as i32);
        let pct = ((qh as f64 / c.height as f64) * 100.0).round() as i32;
        self.queue_pct = pct.clamp(10, 60) as u16;
    }

    pub fn persist_queue_pct(&mut self) {
        self.config.layout.queue_pct = self.queue_pct;
        let _ = self.config.save();
    }

    fn transport_step(&mut self, delta: i32) {
        let n = 5i32;
        self.transport_sel = (((self.transport_sel as i32 + delta) % n + n) % n) as usize;
    }

    fn activate_transport(&mut self) {
        match self.transport_sel {
            0 => self.toggle_shuffle(),
            1 => self.prev_track(),
            2 => self.toggle_pause(),
            3 => self.next_track(true),
            _ => self.cycle_repeat(),
        }
    }

    pub fn cycle_focus(&mut self, forward: bool) {
        self.focus = if forward {
            self.focus.next()
        } else {
            self.focus.prev()
        };
    }

    pub fn set_focus(&mut self, focus: Focus) {
        // Entering the search field drops the cursor at the end of any existing text.
        if focus == Focus::Search {
            self.search_cursor = self.search.chars().count();
        }
        self.focus = focus;
    }

    /// Enter: step right (Search→Artists→Albums→Tracks) or play the selection.
    pub fn activate(&mut self) {
        match self.focus {
            Focus::Search => self.focus = Focus::Artists,
            Focus::Artists => self.focus = Focus::Albums,
            Focus::Albums => self.focus = Focus::Tracks,
            Focus::Tracks => {
                if let Some(i) = self.track_state.selected() {
                    self.queue = self.view_tracks.clone();
                    self.play_index(i);
                }
            }
            Focus::Queue => {
                if let Some(i) = self.queue_state.selected() {
                    self.play_index(i);
                }
            }
            Focus::Seekbar => self.toggle_pause(),
            Focus::Transport => self.activate_transport(),
            Focus::Volume => {}
        }
    }

    /// Start playing whatever is under the focus. Unlike `activate` (which steps
    /// focus rightward for artists/albums), this plays them: artist → all their
    /// tracks, album → its tracks, track/queue → that item. Used for double-click.
    pub fn activate_play(&mut self) {
        let q = self.search.to_lowercase();
        match self.focus {
            Focus::Artists => {
                let artist = self.selected_artist();
                let tracks = self.library.tracks_view(&q, artist.as_deref(), None);
                self.play_tracks(tracks, 0);
            }
            Focus::Albums => {
                let artist = self.selected_artist();
                let album = self.selected_album();
                let tracks = self.library.tracks_view(&q, artist.as_deref(), album.as_ref());
                self.play_tracks(tracks, 0);
            }
            Focus::Tracks => {
                if let Some(i) = self.track_state.selected() {
                    let tracks = self.view_tracks.clone();
                    self.play_tracks(tracks, i);
                }
            }
            Focus::Queue => {
                if let Some(i) = self.queue_state.selected() {
                    self.play_index(i);
                }
            }
            _ => {}
        }
    }

    fn play_tracks(&mut self, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        let start = start.min(tracks.len() - 1);
        self.queue = tracks;
        self.play_index(start);
    }

    /// True if this left click is a double-click (same cell within 400 ms of the
    /// previous one). Resets on a double so a triple click doesn't re-trigger.
    pub fn is_double_click(&mut self, x: u16, y: u16) -> bool {
        let now = Instant::now();
        let double = self
            .last_click
            .map(|(t, lx, ly)| lx == x && ly == y && now.duration_since(t) < Duration::from_millis(400))
            .unwrap_or(false);
        self.last_click = if double { None } else { Some((now, x, y)) };
        double
    }

    // --- queue + playback ------------------------------------------------

    /// Append the current selection (track / whole album / whole artist) to the queue.
    pub fn add_to_queue(&mut self) {
        let q = self.search.to_lowercase();
        let to_add: Vec<Track> = match self.focus {
            Focus::Albums => {
                let artist = self.selected_artist();
                let album = self.selected_album();
                self.library
                    .tracks_view(&q, artist.as_deref(), album.as_ref())
            }
            Focus::Artists => {
                let artist = self.selected_artist();
                self.library.tracks_view(&q, artist.as_deref(), None)
            }
            // Track/Search/anything else: the highlighted track.
            _ => self
                .track_state
                .selected()
                .and_then(|i| self.view_tracks.get(i).cloned())
                .into_iter()
                .collect(),
        };
        if to_add.is_empty() {
            return;
        }
        let n = to_add.len();
        self.queue.extend(to_add);
        if self.queue_state.selected().is_none() {
            self.queue_state.select(Some(0));
        }
        self.notify(format!("Added {n} track(s) to queue."));
    }

    /// Remove the selected item from the queue (bound to `a` while the Queue
    /// panel is focused). If the now-playing entry is removed, the current track
    /// plays out but won't auto-advance.
    pub fn remove_from_queue(&mut self) {
        let Some(i) = self.queue_state.selected() else {
            return;
        };
        if i >= self.queue.len() {
            return;
        }
        self.queue.remove(i);
        self.now = match self.now {
            Some(n) if i < n => Some(n - 1),
            Some(n) if i == n => None,
            other => other,
        };
        if self.queue.is_empty() {
            self.queue_state.select(None);
        } else {
            self.queue_state.select(Some(i.min(self.queue.len() - 1)));
        }
        self.notify("Removed from queue.");
    }

    /// Record the playing track as the persistent status line, clearing any
    /// transient message.
    fn set_now_playing(&mut self, track: &Track) {
        self.now_playing = format!("▶ {} — {}", track.track_artist, track.title);
        self.status = self.now_playing.clone();
        self.notify_until = None;
    }

    /// Show a transient message that reverts to the now-playing line after a few
    /// seconds (handled in `on_tick`).
    fn notify(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
        self.notify_until = Some(Instant::now() + Duration::from_secs(4));
    }

    fn play_index(&mut self, i: usize) {
        let Some(track) = self.queue.get(i).cloned() else {
            return;
        };
        self.now = Some(i);
        self.queue_state.select(Some(i));
        match self.audio.play(&track.path, track.length_secs) {
            Ok(()) => {
                self.set_now_playing(&track);
                self.request_art(track.path.clone());
            }
            Err(e) => {
                self.status = format!("Error: {e}");
            }
        }
    }

    /// Show a track's cover. Cached covers build instantly; otherwise decoding
    /// runs on a worker thread so playback starts without waiting on the (often
    /// multi-MB) embedded image, and the art appears via `poll_art`.
    fn request_art(&mut self, path: PathBuf) {
        self.art_path = Some(path.clone());
        if let Some(img) = self.art_cache.get(&path).cloned() {
            self.set_current_art(Some(img));
            return;
        }
        self.set_current_art(None);
        if !self.art.available() {
            return;
        }
        let tx = self.art_tx.clone();
        std::thread::spawn(move || {
            let image = load_cover_cached(&path);
            let _ = tx.send(ArtMsg { path, image });
        });
    }

    /// Set the current track's cover and (re)build its render protocol(s).
    fn set_current_art(&mut self, img: Option<DynamicImage>) {
        self.art_proto = img.clone().and_then(|i| self.art.protocol_from_image(i));
        if self.show_art_popup {
            self.art_popup_proto = img.clone().and_then(|i| self.art.protocol_from_image(i));
        }
        self.art_image = img;
    }

    fn poll_art(&mut self) {
        let mut pending = Vec::new();
        while let Ok(msg) = self.art_rx.try_recv() {
            pending.push(msg);
        }
        for msg in pending {
            if let Some(img) = &msg.image {
                if self.art_cache.len() > 128 {
                    self.art_cache.clear();
                }
                self.art_cache.insert(msg.path.clone(), img.clone());
            }
            if self.art_path.as_deref() == Some(msg.path.as_path()) {
                self.set_current_art(msg.image.clone());
            }
        }
    }

    /// Pixel dimensions of the current cover, so the popup box can match the
    /// album's aspect ratio (covers aren't always square).
    pub fn popup_art_size(&self) -> Option<(u32, u32)> {
        self.art_image.as_ref().map(|i| (i.width(), i.height()))
    }

    /// Open the large centered album-art overlay, building a dedicated protocol
    /// (separate from the bottom-bar one so the same image isn't rendered to two
    /// areas in one frame).
    pub fn open_art_popup(&mut self) {
        self.show_art_popup = true;
        self.art_popup_proto = self
            .art_image
            .clone()
            .and_then(|i| self.art.protocol_from_image(i));
    }

    /// React to a terminal resize. A font-size change alters the cell grid (and
    /// fires a resize), so re-derive the cell pixel size and re-encode the cover
    /// at the new scale. Terminals that don't report pixel size are a no-op.
    pub fn on_resize(&mut self) {
        let Ok(ws) = crossterm::terminal::window_size() else {
            return;
        };
        if ws.width == 0 || ws.height == 0 || ws.columns == 0 || ws.rows == 0 {
            return;
        }
        let fs = ratatui_image::FontSize::new(ws.width / ws.columns, ws.height / ws.rows);
        if self.art.update_font_size(fs) {
            let img = self.art_image.clone();
            self.set_current_art(img);
        }
    }

    pub fn toggle_pause(&mut self) {
        self.audio.toggle_pause();
    }

    pub fn next_track(&mut self, user: bool) {
        if self.queue.is_empty() {
            return;
        }
        if self.repeat == RepeatMode::One && !user {
            if let Some(i) = self.now {
                self.play_index(i);
            }
            return;
        }
        let len = self.queue.len();
        let cur = self.now.unwrap_or(0);
        let next = if self.shuffle && len > 1 {
            let mut r = (self.next_rand() % len as u64) as usize;
            if r == cur {
                r = (r + 1) % len;
            }
            r
        } else if cur + 1 < len {
            cur + 1
        } else if self.repeat == RepeatMode::All {
            0
        } else {
            self.audio.stop();
            self.now = None;
            return;
        };
        self.play_index(next);
    }

    pub fn prev_track(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        // If more than ~3s in, restart current track instead of going back.
        if self.audio.position().as_secs() > 3
            && let Some(i) = self.now {
                self.play_index(i);
                return;
            }
        let cur = self.now.unwrap_or(0);
        let prev = if cur == 0 {
            if self.repeat == RepeatMode::All {
                self.queue.len() - 1
            } else {
                0
            }
        } else {
            cur - 1
        };
        self.play_index(prev);
    }

    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.notify(format!("Shuffle {}", if self.shuffle { "on" } else { "off" }));
    }

    pub fn cycle_repeat(&mut self) {
        self.repeat = self.repeat.next();
        self.notify(format!("Repeat: {}", self.repeat.glyph()));
    }

    pub fn volume_up(&mut self) {
        self.audio.change_volume(0.05);
    }
    pub fn volume_down(&mut self) {
        self.audio.change_volume(-0.05);
    }
    pub fn set_volume(&mut self, frac: f64) {
        self.audio.set_volume(frac.clamp(0.0, 1.0) as f32);
    }

    /// Empties the play queue. The currently playing track keeps playing to its
    /// end (the decoder is independent of the queue list); it simply won't
    /// auto-advance afterwards.
    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.now = None;
        self.queue_state.select(None);
        self.notify("Queue cleared.");
    }

    pub fn seek_relative(&mut self, secs: i64) {
        if !self.audio.seek_relative(secs) {
            self.notify("Seeking not supported for this track.");
        }
    }

    pub fn seek_fraction(&mut self, frac: f64) {
        if !self.audio.seek_fraction(frac) {
            self.notify("Seeking not supported for this track.");
        }
    }

    /// Called on every drag event: update the visual scrubber immediately and
    /// fire an audio seek at most once per 150 ms to avoid buffer overruns.
    pub fn seek_dragging(&mut self, frac: f64) {
        self.pending_seek = Some(frac);
        let now = std::time::Instant::now();
        let due = self.last_seek_at
            .map(|t| now.duration_since(t).as_millis() >= 150)
            .unwrap_or(true);
        if due {
            self.seek_fraction(frac);
            self.last_seek_at = Some(now);
        }
    }

    // --- search ----------------------------------------------------------

    /// Byte offset of character index `i` in `search` (`search.len()` at the end).
    fn search_byte_at(&self, i: usize) -> usize {
        self.search
            .char_indices()
            .nth(i)
            .map(|(b, _)| b)
            .unwrap_or(self.search.len())
    }

    fn search_char_len(&self) -> usize {
        self.search.chars().count()
    }

    pub fn search_input(&mut self, c: char) {
        let b = self.search_byte_at(self.search_cursor);
        self.search.insert(b, c);
        self.search_cursor += 1;
        self.refresh_views();
    }

    /// Delete the character before the cursor (Backspace / Ctrl-H).
    pub fn search_backspace(&mut self) {
        if self.search_cursor == 0 {
            return;
        }
        let start = self.search_byte_at(self.search_cursor - 1);
        let end = self.search_byte_at(self.search_cursor);
        self.search.replace_range(start..end, "");
        self.search_cursor -= 1;
        self.refresh_views();
    }

    /// Delete the character under the cursor (Delete / Ctrl-D).
    pub fn search_delete_forward(&mut self) {
        if self.search_cursor >= self.search_char_len() {
            return;
        }
        let start = self.search_byte_at(self.search_cursor);
        let end = self.search_byte_at(self.search_cursor + 1);
        self.search.replace_range(start..end, "");
        self.refresh_views();
    }

    pub fn clear_search(&mut self) {
        self.search.clear();
        self.search_cursor = 0;
        self.refresh_views();
    }

    // Cursor motion (no filter change, so no refresh needed).
    pub fn search_home(&mut self) {
        self.search_cursor = 0;
    }
    pub fn search_end(&mut self) {
        self.search_cursor = self.search_char_len();
    }
    pub fn search_move(&mut self, delta: i32) {
        let len = self.search_char_len() as i32;
        self.search_cursor = (self.search_cursor as i32 + delta).clamp(0, len) as usize;
    }

    /// Word-boundary target from the cursor: `dir < 0` moves back over a run of
    /// separators then a run of word chars; `dir > 0` does the same forward.
    fn search_word_boundary(&self, dir: i32) -> usize {
        let chars: Vec<char> = self.search.chars().collect();
        let word = |c: char| c.is_alphanumeric();
        let mut i = self.search_cursor;
        if dir < 0 {
            while i > 0 && !word(chars[i - 1]) {
                i -= 1;
            }
            while i > 0 && word(chars[i - 1]) {
                i -= 1;
            }
        } else {
            let n = chars.len();
            while i < n && !word(chars[i]) {
                i += 1;
            }
            while i < n && word(chars[i]) {
                i += 1;
            }
        }
        i
    }

    pub fn search_move_word(&mut self, dir: i32) {
        self.search_cursor = self.search_word_boundary(dir);
    }

    /// Cut the character range `[from, to)` (char indices) into the kill buffer.
    fn search_kill(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let start = self.search_byte_at(from);
        let end = self.search_byte_at(to);
        self.kill_buffer = self.search[start..end].to_string();
        self.search.replace_range(start..end, "");
        self.search_cursor = self.search_cursor.min(from);
        self.refresh_views();
    }

    pub fn search_kill_word_back(&mut self) {
        let to = self.search_cursor;
        let from = self.search_word_boundary(-1);
        self.search_kill(from, to);
    }
    pub fn search_kill_word_forward(&mut self) {
        let from = self.search_cursor;
        let to = self.search_word_boundary(1);
        self.search_kill(from, to);
        self.search_cursor = from;
    }
    pub fn search_kill_to_start(&mut self) {
        self.search_kill(0, self.search_cursor);
    }
    pub fn search_kill_to_end(&mut self) {
        self.search_kill(self.search_cursor, self.search_char_len());
    }

    /// Insert the kill buffer at the cursor (Ctrl-Y).
    pub fn search_yank(&mut self) {
        if self.kill_buffer.is_empty() {
            return;
        }
        let text = self.kill_buffer.clone();
        let b = self.search_byte_at(self.search_cursor);
        self.search.insert_str(b, &text);
        self.search_cursor += text.chars().count();
        self.refresh_views();
    }

    // --- scanning --------------------------------------------------------

    pub fn start_scan(&mut self) {
        let (tx, rx): (Sender<ScanMsg>, Receiver<ScanMsg>) = std::sync::mpsc::channel();
        scan::spawn(self.config.music_dir.clone(), tx);
        self.scan_rx = Some(rx);
        self.scanning = Some((0, 0));
        self.status = format!("Scanning {}…", self.config.music_dir.display());
    }

    /// Drains scan messages; called each tick. Returns true if anything changed.
    pub fn poll_scan(&mut self) {
        let Some(rx) = &self.scan_rx else { return };
        let mut finished: Option<Vec<Track>> = None;
        let mut err: Option<String> = None;
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ScanMsg::Progress { done, total } => self.scanning = Some((done, total)),
                ScanMsg::Finished(tracks) => finished = Some(tracks),
                ScanMsg::Error(e) => err = Some(e),
            }
        }
        if let Some(e) = err {
            self.status = format!("Scan error: {e}");
            self.scanning = None;
            self.scan_rx = None;
        }
        if let Some(tracks) = finished {
            self.scanning = None;
            self.scan_rx = None;
            self.persist_and_load(tracks);
        }
    }

    fn persist_and_load(&mut self, tracks: Vec<Track>) {
        match db::open().and_then(|mut conn| {
            db::replace_all(&mut conn, &tracks)?;
            db::load_all(&conn)
        }) {
            Ok(loaded) => {
                let n = loaded.len();
                self.library = Library::new(loaded);
                self.refresh_views();
                self.status = format!("Scan complete: {n} tracks.");
            }
            Err(e) => {
                self.library = Library::new(tracks);
                self.refresh_views();
                self.status = format!("Loaded (DB save failed: {e}).");
            }
        }
    }

    pub fn set_music_dir(&mut self, dir: PathBuf) {
        self.config.music_dir = dir;
        let _ = self.config.save();
    }

    pub fn gapless_secs(&self) -> f64 {
        self.config.playback.gapless_fade_secs
    }

    pub fn toggle_big_icons(&mut self) {
        self.big_icons = !self.big_icons;
        self.config.ui.big_transport_icons = self.big_icons;
        let _ = self.config.save();
    }

    pub fn toggle_sort_articles(&mut self) {
        self.config.ui.sort_articles = !self.config.ui.sort_articles;
        let _ = self.config.save();
        self.refresh_views();
    }

    pub fn toggle_cache_all_art(&mut self) {
        self.cache_all_art = !self.cache_all_art;
        self.config.ui.cache_all_art = self.cache_all_art;
        let _ = self.config.save();
        if self.cache_all_art {
            self.start_art_precache();
            self.notify("Caching all album art in background…");
        } else {
            self.notify("Album-art pre-caching off.");
        }
    }

    /// Build cover thumbnails for the whole library on a background thread so
    /// they're cached on disk and load instantly later.
    pub fn start_art_precache(&self) {
        let paths: Vec<PathBuf> = self.library.tracks.iter().map(|t| t.path.clone()).collect();
        std::thread::spawn(move || {
            for p in paths {
                // Skip the decode work entirely if the thumbnail is already on disk.
                if art_thumb_path(&p).exists() {
                    continue;
                }
                let _ = load_cover_cached(&p);
            }
        });
    }

    // --- playlists -------------------------------------------------------

    fn clamp_pl_states(&mut self) {
        clamp(&mut self.playlist_state, self.playlists.len());
        let sel = self.playlist_state.selected().unwrap_or(0);
        let n = self.playlists.get(sel).map(|p| p.tracks.len()).unwrap_or(0);
        clamp(&mut self.pl_track_state, n);
    }

    pub fn selected_playlist(&self) -> Option<&Playlist> {
        self.playlist_state
            .selected()
            .and_then(|i| self.playlists.get(i))
    }

    pub fn pl_move(&mut self, delta: i32) {
        match self.pl_focus {
            PlPane::List => {
                move_state(&mut self.playlist_state, self.playlists.len(), delta);
                let sel = self.playlist_state.selected().unwrap_or(0);
                let has = self
                    .playlists
                    .get(sel)
                    .map(|p| !p.tracks.is_empty())
                    .unwrap_or(false);
                self.pl_track_state.select(if has { Some(0) } else { None });
            }
            PlPane::Tracks => {
                let n = self
                    .playlist_state
                    .selected()
                    .and_then(|i| self.playlists.get(i))
                    .map(|p| p.tracks.len())
                    .unwrap_or(0);
                move_state(&mut self.pl_track_state, n, delta);
            }
        }
    }

    pub fn pl_set_pane(&mut self, pane: PlPane) {
        self.pl_focus = pane;
    }

    /// Enter in the Playlists tab: play the selected playlist (from the chosen
    /// track when the track pane is focused).
    pub fn pl_activate(&mut self) {
        let start = match self.pl_focus {
            PlPane::Tracks => self.pl_track_state.selected().unwrap_or(0),
            PlPane::List => 0,
        };
        let tracks = match self.selected_playlist() {
            Some(p) if !p.tracks.is_empty() => p.tracks.clone(),
            Some(_) => {
                self.status = "Playlist is empty.".into();
                return;
            }
            None => return,
        };
        self.queue = tracks;
        self.play_index(start.min(self.queue.len() - 1));
    }

    pub fn begin_new_playlist(&mut self) {
        if self.queue.is_empty() {
            self.status = "Queue is empty — nothing to save as a playlist.".into();
            return;
        }
        self.editing_playlist_name = Some(String::new());
    }
    pub fn pl_name_input(&mut self, c: char) {
        if let Some(b) = &mut self.editing_playlist_name {
            b.push(c);
        }
    }
    pub fn pl_name_backspace(&mut self) {
        if let Some(b) = &mut self.editing_playlist_name {
            b.pop();
        }
    }
    pub fn cancel_new_playlist(&mut self) {
        self.editing_playlist_name = None;
    }
    pub fn commit_new_playlist(&mut self) {
        let Some(name) = self.editing_playlist_name.take() else {
            return;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let n = self.queue.len();
        self.playlists.push(Playlist {
            name: name.clone(),
            tracks: self.queue.clone(),
        });
        playlist::save(&self.playlists);
        self.playlist_state.select(Some(self.playlists.len() - 1));
        self.clamp_pl_states();
        self.status = format!("Saved playlist '{name}' ({n} tracks).");
    }

    pub fn append_queue_to_playlist(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let q = self.queue.clone();
        if let Some(i) = self.playlist_state.selected()
            && let Some(pl) = self.playlists.get_mut(i) {
                let n = q.len();
                pl.tracks.extend(q);
                let name = pl.name.clone();
                playlist::save(&self.playlists);
                self.clamp_pl_states();
                self.status = format!("Added {n} track(s) to '{name}'.");
            }
    }

    pub fn delete_playlist(&mut self) {
        if let Some(i) = self.playlist_state.selected()
            && i < self.playlists.len() {
                let name = self.playlists.remove(i).name;
                playlist::save(&self.playlists);
                self.clamp_pl_states();
                self.status = format!("Deleted playlist '{name}'.");
            }
    }

    pub fn remove_pl_track(&mut self) {
        if let (Some(pi), Some(ti)) = (
            self.playlist_state.selected(),
            self.pl_track_state.selected(),
        )
            && let Some(pl) = self.playlists.get_mut(pi)
                && ti < pl.tracks.len() {
                    pl.tracks.remove(ti);
                    playlist::save(&self.playlists);
                    self.clamp_pl_states();
                }
    }

    /// Adjust the gapless-playback crossfade duration (Settings tab), applying
    /// it live and persisting it.
    pub fn adjust_gapless(&mut self, delta: f64) {
        let v = (self.config.playback.gapless_fade_secs + delta).clamp(0.0, 12.0);
        self.config.playback.gapless_fade_secs = v;
        self.audio.set_crossfade(v as f32);
        let _ = self.config.save();
        let msg = if v <= 0.0 {
            "Gapless playback: off".to_string()
        } else {
            format!("Gapless playback: {v:.1}s fade")
        };
        self.notify(msg);
    }

    // --- library-tab path editing ---------------------------------------

    pub fn begin_edit_path(&mut self) {
        self.editing_path = Some(self.config.music_dir.to_string_lossy().into_owned());
    }
    pub fn edit_path_input(&mut self, c: char) {
        if let Some(buf) = &mut self.editing_path {
            buf.push(c);
        }
    }
    pub fn edit_path_backspace(&mut self) {
        if let Some(buf) = &mut self.editing_path {
            buf.pop();
        }
    }
    pub fn commit_edit_path(&mut self) {
        if let Some(buf) = self.editing_path.take() {
            let trimmed = buf.trim();
            if !trimmed.is_empty() {
                self.set_music_dir(PathBuf::from(shellexpand_home(trimmed)));
                self.notify(format!("Music dir set to {}", self.config.music_dir.display()));
            }
        }
    }
    pub fn cancel_edit_path(&mut self) {
        self.editing_path = None;
    }

    // --- per-frame tick --------------------------------------------------

    /// Returns true when the UI should be redrawn this tick.
    pub fn on_tick(&mut self) -> bool {
        self.poll_scan();
        self.poll_art();
        self.audio.update();

        // Start an automatic crossfade into the next track near the end.
        if self.audio.is_playing() && self.audio.should_start_crossfade()
            && let Some(i) = self.peek_next_index() {
                let track = self.queue[i].clone();
                if (track.length_secs as f32) > self.audio.crossfade_secs()
                    && self.audio.crossfade_to(&track.path, track.length_secs)
                {
                    self.now = Some(i);
                    self.queue_state.select(Some(i));
                    self.set_now_playing(&track);
                    self.request_art(track.path.clone());
                }
            }

        if self.audio.finished() {
            self.next_track(false);
        }

        // The art popup is static; don't re-render it on every tick (each kitty
        // re-placement of a large image is wasteful). Events still redraw.
        let mut dirty =
            !self.show_art_popup && (self.audio.is_playing() || self.scanning.is_some());
        // Revert an expired transient message back to the now-playing line.
        if let Some(until) = self.notify_until
            && Instant::now() >= until {
                self.status = self.now_playing.clone();
                self.notify_until = None;
                dirty = true;
            }
        dirty
    }

    /// The index the queue would advance to next, without playing it (used to
    /// preload the crossfade target). `None` when there's nothing to fade into.
    fn peek_next_index(&mut self) -> Option<usize> {
        if self.queue.is_empty() || self.repeat == RepeatMode::One {
            return None;
        }
        let len = self.queue.len();
        let cur = self.now?;
        if self.shuffle && len > 1 {
            let mut r = (self.next_rand() % len as u64) as usize;
            if r == cur {
                r = (r + 1) % len;
            }
            Some(r)
        } else if cur + 1 < len {
            Some(cur + 1)
        } else if self.repeat == RepeatMode::All {
            Some(0)
        } else {
            None
        }
    }
}

/// Expand a leading `~` to the user's home directory.
fn shellexpand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()) {
            return home.join(rest).to_string_lossy().into_owned();
        }
    p.to_string()
}

/// Cover-art thumbnail edge in pixels. Large enough that the big art popup still
/// looks crisp, while staying cheap to decode and terminal-encode.
const ART_THUMB_PX: u32 = 1024;

/// Load a track's cover, preferring a persistent on-disk thumbnail. On a miss,
/// extract the embedded art from the audio file, downscale, and cache the
/// thumbnail so subsequent loads (and future sessions) are fast.
fn art_thumb_path(path: &std::path::Path) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&path, &mut hasher);
    // The thumbnail size is part of the filename so bumping it invalidates old
    // (smaller) cached thumbnails instead of reusing them.
    crate::config::art_cache_dir().join(format!(
        "{:016x}-{ART_THUMB_PX}.png",
        std::hash::Hasher::finish(&hasher)
    ))
}

fn load_cover_cached(path: &std::path::Path) -> Option<DynamicImage> {
    let thumb = art_thumb_path(path);

    if let Ok(bytes) = std::fs::read(&thumb)
        && let Ok(img) = image::load_from_memory(&bytes) {
            return Some(img);
        }

    let raw = scan::read_cover(path)?;
    let img = image::load_from_memory(&raw).ok()?;
    let small = if img.width() > ART_THUMB_PX || img.height() > ART_THUMB_PX {
        img.resize(ART_THUMB_PX, ART_THUMB_PX, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let _ = std::fs::create_dir_all(crate::config::art_cache_dir());
    let _ = small.save(&thumb);
    Some(small)
}

fn clamp(state: &mut ListState, len: usize) {
    if len == 0 {
        state.select(None);
    } else {
        let sel = state.selected().unwrap_or(0).min(len - 1);
        state.select(Some(sel));
    }
}

fn move_state(state: &mut ListState, len: usize, delta: i32) {
    if len == 0 {
        state.select(None);
        return;
    }
    let cur = state.selected().unwrap_or(0) as i32;
    let new = (cur + delta).clamp(0, len as i32 - 1);
    state.select(Some(new as usize));
}
