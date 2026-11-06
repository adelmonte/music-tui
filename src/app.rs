use std::collections::{HashMap, HashSet};
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
use crate::deezer;
use crate::spotify;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tab {
    Local,
    Streaming,
    Browser,
    Playlists,
    Settings,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Local => "Local",
            Tab::Streaming => "Spotify",
            Tab::Browser => "Explore",
            Tab::Playlists => "Local Playlists",
            Tab::Settings => "Settings",
        }
    }
}

/// Track order within each Spotify-tab list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpSort {
    /// Reverse of the export's order: the export lists oldest additions first.
    Recent,
    Oldest,
    Title,
    Artist,
    Album,
}

impl SpSort {
    pub fn label(self) -> &'static str {
        match self {
            SpSort::Recent => "newest first",
            SpSort::Oldest => "oldest first",
            SpSort::Title => "by title",
            SpSort::Artist => "by artist",
            SpSort::Album => "by album",
        }
    }
    fn next(self) -> SpSort {
        match self {
            SpSort::Recent => SpSort::Oldest,
            SpSort::Oldest => SpSort::Title,
            SpSort::Title => SpSort::Artist,
            SpSort::Artist => SpSort::Album,
            SpSort::Album => SpSort::Recent,
        }
    }
}

/// What's saved between runs: the play queue and position, playback modes,
/// and where the user was in the UI. Enums are stored by name so a renamed
/// or removed variant just falls back to the default.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Session {
    #[serde(default)]
    tab: String,
    #[serde(default)]
    queue: Vec<Track>,
    #[serde(default)]
    now: Option<usize>,
    #[serde(default)]
    position_secs: f64,
    #[serde(default)]
    volume: Option<f32>,
    #[serde(default)]
    shuffle: bool,
    #[serde(default)]
    repeat: String,
    #[serde(default)]
    search: String,
    #[serde(default)]
    artist: Option<usize>,
    #[serde(default)]
    album: Option<usize>,
    #[serde(default)]
    track: Option<usize>,
    #[serde(default)]
    queue_sel: Option<usize>,
    #[serde(default)]
    sp_list: Option<usize>,
    #[serde(default)]
    sp_track: Option<usize>,
    #[serde(default)]
    sp_tracks_focused: bool,
    #[serde(default)]
    sp_sort: String,
    /// Each tab's search text, by tab name.
    #[serde(default)]
    searches: std::collections::BTreeMap<String, String>,
}

/// One row of the Settings tab. Headers aren't selectable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingRow {
    Header(&'static str),
    MusicDir,
    Rescan,
    Gapless,
    KeepGoing,
    CacheAllArt,
    SortArticles,
    Thumbnails,
    BigIcons,
    TabSpotify,
    TabExplore,
    TabPlaylists,
    SharedSearch,
    SpotifyCsv,
    SpotifyReimport,
    SpotifyLinks,
    DownloadsDir,
    SpotifyCoversDir,
    LocalArtDir,
    LibraryFile,
    ConfigFile,
}

impl SettingRow {
    pub fn selectable(self) -> bool {
        !matches!(self, SettingRow::Header(_))
    }
}

/// Disk usage of a cache directory: (files, bytes).
#[derive(Clone, Copy, Default)]
pub struct DirStats {
    pub files: usize,
    pub bytes: u64,
}

/// Which path the Settings tab's text field is editing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathField {
    MusicDir,
    SpotifyCsv,
}

/// Which pane is active within the Playlists tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlPane {
    List,
    Tracks,
    /// The queue under the songs (Spotify and Explore tabs).
    Queue,
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
    /// Scrubbing a column's scrollbar thumb.
    Scroll(ScrollList),
    /// Resizing the Spotify tab's list/tracks split.
    SpDivider,
}

/// Which scrollable list a `Drag::Scroll` is scrubbing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollList {
    Artists,
    Albums,
    Tracks,
    Queue,
    PlList,
    PlTracks,
    Settings,
    SpList,
    SpTracks,
    BrSections,
    BrItems,
}

/// A decoded cover image returned from the background art worker, tagged with
/// the track path it belongs to so stale results can be discarded.
struct ArtMsg {
    path: PathBuf,
    image: Option<DynamicImage>,
}

/// A decoded, row-thumbnail-sized cover for the Albums column, tagged with the
/// album it belongs to. Kept on a channel separate from `ArtMsg` (own thread,
/// own cache) since it's a different-shaped background job — one image per
/// visible row rather than one for the whole app.
struct RowThumbMsg {
    album: AlbumRef,
    image: Option<DynamicImage>,
}

/// One row-thumbnail load.
struct ThumbJob {
    key: AlbumRef,
    srcs: Vec<ThumbSrc>,
}

/// Background row-thumbnail workers. Jobs are served newest first, so the
/// rows on screen now load before ones scrolled past; the oldest are
/// dropped once too many pile up.
struct ThumbPool {
    jobs: std::sync::Arc<(std::sync::Mutex<Vec<ThumbJob>>, std::sync::Condvar)>,
}

impl ThumbPool {
    const MAX_QUEUED: usize = 96;

    fn new(tx: Sender<RowThumbMsg>) -> Self {
        let jobs: std::sync::Arc<(std::sync::Mutex<Vec<ThumbJob>>, std::sync::Condvar)> = Default::default();
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 6);
        for _ in 0..workers {
            let (jobs, tx) = (jobs.clone(), tx.clone());
            std::thread::spawn(move || {
                loop {
                    let job = {
                        let (lock, ready) = &*jobs;
                        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(j) = q.pop() {
                                break j;
                            }
                            q = ready.wait(q).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let image = row_thumb(&job.key, &job.srcs);
                    if tx.send(RowThumbMsg { album: job.key, image }).is_err() {
                        return;
                    }
                }
            });
        }
        Self { jobs }
    }

    /// Queue a job; returns the keys of jobs dropped to make room.
    fn push(&self, job: ThumbJob) -> Vec<AlbumRef> {
        let (lock, ready) = &*self.jobs;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        q.push(job);
        let excess = q.len().saturating_sub(Self::MAX_QUEUED);
        let dropped = q.drain(..excess).map(|j| j.key).collect();
        ready.notify_one();
        dropped
    }
}

/// A linked Spotify playlist read in the background.
struct LinkMsg {
    link: String,
    result: Result<Playlist, String>,
}

/// Results of background Browser-tab (Deezer) requests.
enum BrMsg {
    Home(Vec<deezer::View>),
    ForYou(deezer::View),
    Results(String, Vec<deezer::View>),
    Opened(deezer::View),
    /// A song radio: shown in Explore, or (`extend`) appended to the queue
    /// by keep-going mode.
    Radio { view: deezer::View, extend: bool },
    RadioFailed { title: String, error: String, extend: bool },
    Error(String),
}

/// Where a row thumbnail's image comes from.
#[derive(Clone)]
pub enum ThumbSrc {
    /// A track's cover (embedded, or a Streaming track's fetched cover).
    Track(PathBuf),
    Url(String),
}

/// Where the add-to menu can put tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddTarget {
    /// A Streaming-library list, by name.
    Store(String),
    /// A Playlists-tab playlist, by index.
    Local(usize),
    New,
}

/// The `m` "add to playlist" popup.
/// What the "Go to…" menu offers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GotoKind {
    Radio,
    Artist,
    Album,
    KeepGoing,
}

/// What a confirmation does when answered yes.
pub enum ConfirmAction {
    Reject(Track),
    Unlike(Track),
    SpDelete,
    DeletePlaylist,
}

/// A yes/no question before something destructive.
pub struct Confirm {
    pub title: &'static str,
    pub subject: String,
    pub detail: String,
    pub yes: &'static str,
    pub no: &'static str,
    action: ConfirmAction,
}

/// The `g` "Go to…" menu, for one track.
pub struct GotoMenu {
    pub track: Track,
    pub sel: usize,
}

pub struct AddMenu {
    pub tracks: Vec<Track>,
    pub sel: usize,
    /// Some(buffer) while naming a new list.
    pub naming: Option<String>,
}

/// Result of a background YouTube fetch (or re-filing) for a Spotify track:
/// the track with its metadata filled in, or an error message.
struct FetchMsg {
    path: PathBuf,
    result: Result<Track, String>,
}

/// Layout rectangles recorded each frame so the input layer can hit-test mouse
/// clicks. The list rects are the *inner* content areas (inside borders).
#[derive(Default, Clone)]
pub struct Rects {
    /// Clickable key hints in the hint bar, and the key each one presses.
    pub hints: Vec<(Rect, crossterm::event::KeyCode)>,
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
    /// Each visible tab-bar label and the tab it selects.
    pub tabs: Vec<(Tab, Rect)>,
    pub pl_list: Rect,
    pub pl_tracks: Rect,
    pub sp_list: Rect,
    pub sp_tracks: Rect,
    pub br_sections: Rect,
    pub br_items: Rect,
    pub settings: Rect,
    /// The Spotify tab body and the x of its draggable pane divider.
    pub sp_body: Rect,
    pub sp_div: u16,
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
    /// Whether the Albums column shows a per-row cover thumbnail.
    pub show_album_thumbnails: bool,
    pub search: String,
    /// Other tabs' search text while they're not shown (unless shared).
    searches: HashMap<Tab, String>,
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

    // Streaming tab state. `sp_store` is the persisted library (imported
    // export + likes/additions); `sp_linked` are linked Spotify playlists, in
    // config order. `spotify_lists` is both combined, sorted.
    sp_store: Vec<Playlist>,
    sp_linked: Vec<(String, Playlist)>,
    /// Paths of every track in Liked Songs, for the ♥ markers.
    pub liked: HashSet<PathBuf>,
    /// Some(buffer) while pasting a Spotify playlist link.
    pub link_input: Option<String>,
    pub goto_menu: Option<GotoMenu>,
    /// A yes/no confirmation waiting for an answer.
    pub confirm: Option<Confirm>,
    /// Song radios, shown as Explore sections (newest first), and the title
    /// of the one still being built.
    pub br_radios: Vec<deezer::View>,
    pub br_radio_building: Option<String>,
    pub add_menu: Option<AddMenu>,
    link_tx: Sender<LinkMsg>,
    link_rx: Receiver<LinkMsg>,
    pub spotify_lists: Vec<Playlist>,
    /// `spotify_lists` as shown: sorted, and filtered by the search box. The
    /// tab's selection states index into this.
    pub sp_view: Vec<Playlist>,
    pub sp_list_state: ListState,
    pub sp_track_state: ListState,
    pub sp_focus: PlPane,
    pub sp_sort: SpSort,
    /// List-pane width percentage in the Spotify tab.
    pub sp_pct: u16,
    /// Why the last import produced nothing (shown in the empty Spotify tab).
    pub spotify_error: Option<String>,
    /// Spotify tracks with a YouTube fetch in flight.
    pub fetching: HashSet<PathBuf>,
    /// The queued Spotify track waiting on its fetch before it can start.
    pending_play: Option<PathBuf>,
    fetch_tx: Sender<FetchMsg>,
    fetch_rx: Receiver<FetchMsg>,

    // Browser tab state. Sections (left) are search results, then the home
    // sections; `br_stack` holds opened albums/artists/playlists (right pane
    // shows the top one, else the selected section).
    pub br_results: Vec<deezer::View>,
    pub br_home: Vec<deezer::View>,
    pub br_stack: Vec<deezer::View>,
    pub br_sec_state: ListState,
    pub br_item_state: ListState,
    pub br_focus: PlPane,
    /// What's loading, shown in the pane title.
    pub br_loading: Option<String>,
    br_home_requested: bool,
    /// The query last sent to Deezer, and the search text as of when it last
    /// changed (searches fire once typing pauses).
    br_query: String,
    br_seen: (String, Instant),
    br_tx: Sender<BrMsg>,
    br_rx: Receiver<BrMsg>,

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
    /// Some(buffer) when editing a path in the Settings tab.
    pub editing_path: Option<String>,
    /// Which path `editing_path` will be committed to.
    pub editing_field: PathField,
    /// Selected row in the Settings tab (an index into `settings_rows()`).
    pub settings_state: ListState,
    /// The session as last written, so periodic saves skip unchanged state.
    last_session: String,
    last_session_at: Instant,
    /// The list whose covers are being prefetched to disk, and the job
    /// generation (bumping it stops the running job).
    cover_job: String,
    cover_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Keep-going mode: a radio is being built to extend the queue, and the
    /// track the last one was built from (so each track seeds at most one).
    radio_extending: bool,
    radio_seed: Option<PathBuf>,
    /// Sizes of the downloaded-music and cover caches, refreshed every few
    /// seconds while the Settings tab is open.
    pub cache_stats: Option<(DirStats, DirStats, DirStats)>,
    cache_stats_at: Option<Instant>,

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

    /// Render protocols for the Albums-column row thumbnails, keyed by album so
    /// scrolling back to a previously-seen row is instant.
    pub row_thumb_cache: HashMap<AlbumRef, StatefulProtocol>,
    /// Albums with a thumbnail request already in flight, so scrolling back and
    /// forth doesn't queue duplicate decodes.
    row_thumb_inflight: HashSet<AlbumRef>,
    /// Albums confirmed to have no embedded cover, so they aren't re-requested
    /// (and re-decoded from disk) on every tick they stay in view.
    row_thumb_no_art: HashSet<AlbumRef>,
    row_thumb_rx: Receiver<RowThumbMsg>,
    thumb_pool: ThumbPool,
    /// Cached row thumbnails, oldest first, for eviction.
    row_thumb_order: std::collections::VecDeque<AlbumRef>,

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
        let show_album_thumbnails = config.ui.show_album_thumbnails;
        let playlists = playlist::load();
        let (art_tx, art_rx) = channel::<ArtMsg>();
        let (row_thumb_tx, row_thumb_rx) = channel::<RowThumbMsg>();
        let (fetch_tx, fetch_rx) = channel::<FetchMsg>();
        let (link_tx, link_rx) = channel::<LinkMsg>();
        let (br_tx, br_rx) = channel::<BrMsg>();
        let mut app = App {
            config,
            theme,
            library,
            audio,
            art,
            tab: Tab::Local,
            dragging: None,
            focus: Focus::Artists,
            transport_sel: 2,
            col_widths,
            big_icons,
            queue_pct,
            cache_all_art,
            show_album_thumbnails,
            search: String::new(),
            search_cursor: 0,
            searches: HashMap::new(),
            kill_buffer: String::new(),
            playlists,
            playlist_state: ListState::default(),
            pl_track_state: ListState::default(),
            pl_focus: PlPane::List,
            editing_playlist_name: None,
            sp_store: Vec::new(),
            sp_linked: Vec::new(),
            liked: HashSet::new(),
            link_input: None,
            goto_menu: None,
            confirm: None,
            br_radios: Vec::new(),
            br_radio_building: None,
            add_menu: None,
            link_tx,
            link_rx,
            spotify_lists: Vec::new(),
            sp_view: Vec::new(),
            sp_list_state: ListState::default(),
            sp_track_state: ListState::default(),
            sp_focus: PlPane::List,
            sp_sort: SpSort::Recent,
            sp_pct: 0,
            spotify_error: None,
            fetching: HashSet::new(),
            pending_play: None,
            fetch_tx,
            fetch_rx,
            br_results: Vec::new(),
            br_home: Vec::new(),
            br_stack: Vec::new(),
            br_sec_state: ListState::default(),
            br_item_state: ListState::default(),
            br_focus: PlPane::List,
            br_loading: None,
            br_home_requested: false,
            br_query: String::new(),
            br_seen: (String::new(), Instant::now()),
            br_tx,
            br_rx,
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
            editing_field: PathField::MusicDir,
            settings_state: ListState::default().with_selected(Some(1)),
            cache_stats: None,
            cache_stats_at: None,
            last_session: String::new(),
            last_session_at: Instant::now(),
            cover_job: String::new(),
            cover_gen: Default::default(),
            radio_extending: false,
            radio_seed: None,
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
            row_thumb_cache: HashMap::new(),
            row_thumb_inflight: HashSet::new(),
            row_thumb_no_art: HashSet::new(),
            thumb_pool: ThumbPool::new(row_thumb_tx),
            row_thumb_rx,
            row_thumb_order: Default::default(),
            rng: seed,
            scan_rx: None,
        };
        app.refresh_views();
        app.clamp_pl_states();
        app.sp_pct = app.config.layout.spotify_pct.clamp(12, 70);
        if app.config.spotify.enabled {
            app.load_spotify();
        }
        if app.library.is_empty() {
            app.status = "Library empty — press 3 for settings, then r to scan.".into();
        } else {
            app.status = format!("{} tracks loaded.", app.library.tracks.len());
        }
        prune_legacy_art_cache();
        if app.cache_all_art {
            app.start_art_precache();
        }
        app.restore_session();
        app
    }

    // --- session ---------------------------------------------------------

    fn session(&self) -> Session {
        let position_secs = match self.now.and_then(|i| self.queue.get(i)) {
            Some(t) if self.audio.current_path() == Some(t.path.as_path()) => {
                self.audio.position().as_secs_f64()
            }
            _ => 0.0,
        };
        Session {
            tab: format!("{:?}", self.tab),
            queue: self.queue.clone(),
            now: self.now,
            position_secs,
            volume: Some(self.audio.volume()),
            shuffle: self.shuffle,
            repeat: format!("{:?}", self.repeat),
            search: self.search_for(Tab::Local),
            searches: [Tab::Local, Tab::Streaming, Tab::Browser, Tab::Playlists]
                .into_iter()
                .map(|t| (format!("{t:?}"), self.search_for(t)))
                .filter(|(_, q)| !q.is_empty())
                .collect(),
            artist: self.artist_state.selected(),
            album: self.album_state.selected(),
            track: self.track_state.selected(),
            queue_sel: self.queue_state.selected(),
            sp_list: self.sp_list_state.selected(),
            sp_track: self.sp_track_state.selected(),
            sp_tracks_focused: self.sp_focus == PlPane::Tracks,
            sp_sort: format!("{:?}", self.sp_sort),
        }
    }

    /// Write the session (only when it changed). Called on quit, and every
    /// few seconds from `on_tick` so a killed terminal loses little.
    pub fn save_session(&mut self) {
        let Ok(body) = toml::to_string(&self.session()) else { return };
        if body == self.last_session {
            return;
        }
        let path = crate::config::session_path();
        let _ = crate::config::ensure_parent(&path);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
            self.last_session = body;
        }
    }

    /// Put the UI and queue back as the last run left them, with the track
    /// that was playing loaded at its position, paused.
    fn restore_session(&mut self) {
        let Some(s) = std::fs::read_to_string(crate::config::session_path())
            .ok()
            .and_then(|b| toml::from_str::<Session>(&b).ok())
        else {
            return;
        };
        if let Some(tab) = self.tabs().into_iter().find(|t| format!("{t:?}") == s.tab) {
            self.tab = tab;
        }
        // Each tab's own search (older sessions had one, the Local tab's).
        let mut searches = s.searches.clone();
        if searches.is_empty() && !s.search.is_empty() {
            searches.insert("Local".into(), s.search.clone());
        }
        for (name, q) in searches {
            if let Some(tab) = [Tab::Local, Tab::Streaming, Tab::Browser, Tab::Playlists]
                .into_iter()
                .find(|t| format!("{t:?}") == name)
            {
                self.searches.insert(tab, q);
            }
        }
        self.shuffle = s.shuffle;
        self.repeat = match s.repeat.as_str() {
            "All" => RepeatMode::All,
            "One" => RepeatMode::One,
            _ => RepeatMode::Off,
        };
        if let Some(v) = s.volume {
            self.audio.set_volume(v);
        }
        self.sp_sort = [SpSort::Recent, SpSort::Oldest, SpSort::Title, SpSort::Artist, SpSort::Album]
            .into_iter()
            .find(|o| format!("{o:?}") == s.sp_sort)
            .unwrap_or(SpSort::Recent);
        self.apply_sp_sort();

        // Browser columns: each selection re-derives the columns to its right.
        self.search = if self.config.ui.shared_search {
            s.search
        } else {
            self.searches.remove(&self.tab).unwrap_or_default()
        };
        self.search_cursor = self.search.chars().count();
        self.artist_state.select(s.artist);
        self.refresh_views();
        self.album_state.select(s.album);
        self.refresh_views();
        self.track_state.select(s.track);
        self.refresh_views();
        self.sp_list_state.select(s.sp_list);
        self.refresh_sp_view();
        self.sp_track_state.select(s.sp_track);
        self.refresh_sp_view();
        self.sp_focus = if s.sp_tracks_focused { PlPane::Tracks } else { PlPane::List };
        // Pane sizes aren't known before the first frame; start each list a
        // few rows above its selection so it's in view.
        for state in [
            &mut self.artist_state,
            &mut self.album_state,
            &mut self.track_state,
            &mut self.sp_list_state,
            &mut self.sp_track_state,
        ] {
            *state.offset_mut() = state.selected().unwrap_or(0).saturating_sub(3);
        }

        self.queue = s.queue;
        self.queue_state.select(s.queue_sel.filter(|&i| i < self.queue.len()));
        let Some(i) = s.now.filter(|&i| i < self.queue.len()) else { return };
        self.now = Some(i);
        let track = self.queue[i].clone();
        if !track.path.exists() || self.audio.play(&track.path, track.length_secs).is_err() {
            return;
        }
        self.audio.toggle_pause();
        if s.position_secs > 1.0 {
            self.audio.seek_to(Duration::from_secs_f64(s.position_secs));
        }
        self.set_now_playing(&track);
        self.now_playing = format!("⏸ {} — {}  (Space to resume)", track.title, track.track_artist);
        self.status = self.now_playing.clone();
        self.request_art(track.path.clone());
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
        let q = self.search_for(Tab::Local).to_lowercase();
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
        self.refresh_sp_view();
    }

    /// Rebuild the Spotify tab's view from the search box: a list whose name
    /// matches keeps all its tracks; otherwise it keeps only the tracks whose
    /// title/artist/album match, and drops out if none do.
    pub fn refresh_sp_view(&mut self) {
        let q = self.search_for(Tab::Streaming).to_lowercase();
        let q = q.trim();
        let hit = |t: &Track| {
            [&t.title, &t.track_artist, &t.album]
                .iter()
                .any(|f| f.to_lowercase().contains(q))
        };
        self.sp_view = self
            .spotify_lists
            .iter()
            .filter_map(|p| {
                if q.is_empty() || p.name.to_lowercase().contains(q) {
                    return Some(p.clone());
                }
                let tracks: Vec<Track> = p.tracks.iter().filter(|t| hit(t)).cloned().collect();
                (!tracks.is_empty()).then(|| Playlist { name: p.name.clone(), tracks })
            })
            .collect();
        clamp(&mut self.sp_list_state, self.sp_view.len());
        let n = self.selected_sp_list().map(|p| p.tracks.len()).unwrap_or(0);
        clamp(&mut self.sp_track_state, n);
    }

    // --- navigation ------------------------------------------------------

    pub fn is_searching(&self) -> bool {
        self.focus == Focus::Search
    }

    pub fn move_selection(&mut self, delta: i32) {
        let album_row_h = if self.show_album_thumbnails { Self::ALBUM_ROW_HEIGHT } else { 1 };
        let (state, len, rect, row_h) = match self.focus {
            Focus::Artists => (&mut self.artist_state, self.view_artists.len(), self.rects.artists, 1),
            Focus::Albums => (&mut self.album_state, self.view_albums.len(), self.rects.albums, album_row_h),
            Focus::Tracks => (&mut self.track_state, self.view_tracks.len(), self.rects.tracks, 1),
            Focus::Queue => (&mut self.queue_state, self.queue.len(), self.rects.queue, 1),
            _ => return,
        };
        move_state(state, len, delta);
        scroll_into_view(state, (rect.height / row_h) as usize);
        // Moving the artist or album highlight re-derives the columns to the right.
        if matches!(self.focus, Focus::Artists | Focus::Albums) {
            self.refresh_views();
        }
    }

    /// Directly pan a list's viewport, independent of its current selection —
    /// used while a scrollbar thumb is being dragged (see `ScrollList` /
    /// `Drag::Scroll` in input.rs). Deliberately does not touch `selected`:
    /// dragging the scrollbar should only change what's visible, not which
    /// row is selected/filtering-on (an Artists/Albums drag shouldn't
    /// re-filter the columns to the right just because you scrolled past).
    pub fn scroll_offset(&mut self, target: ScrollList, offset: usize) {
        let state = match target {
            ScrollList::Artists => &mut self.artist_state,
            ScrollList::Albums => &mut self.album_state,
            ScrollList::Tracks => &mut self.track_state,
            ScrollList::Queue => &mut self.queue_state,
            ScrollList::PlList => &mut self.playlist_state,
            ScrollList::PlTracks => &mut self.pl_track_state,
            ScrollList::Settings => &mut self.settings_state,
            ScrollList::SpList => &mut self.sp_list_state,
            ScrollList::SpTracks => &mut self.sp_track_state,
            ScrollList::BrSections => &mut self.br_sec_state,
            ScrollList::BrItems => &mut self.br_item_state,
        };
        *state.offset_mut() = offset;
    }

    pub fn move_column(&mut self, right: bool) {
        let next = if right { self.focus.right() } else { self.focus.left() };
        if next != Focus::Queue || self.config.ui.show_queue {
            self.focus = next;
        }
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

    /// Tab / Shift-Tab, the same in every tab: search → the tab's panes →
    /// queue (when shown) → progress bar → playback buttons → volume.
    pub fn cycle_focus(&mut self, forward: bool) {
        #[derive(Clone, Copy, PartialEq)]
        enum Stop {
            F(Focus),
            P(PlPane),
        }
        let queue = self.config.ui.show_queue;
        let mut order = Vec::new();
        if self.tab != Tab::Settings {
            order.push(Stop::F(Focus::Search));
        }
        match self.tab {
            Tab::Local => {
                order.extend([Focus::Artists, Focus::Albums, Focus::Tracks].map(Stop::F));
                if queue {
                    order.push(Stop::F(Focus::Queue));
                }
            }
            Tab::Streaming | Tab::Browser => {
                order.extend([PlPane::List, PlPane::Tracks].map(Stop::P));
                if queue {
                    order.push(Stop::P(PlPane::Queue));
                }
            }
            Tab::Playlists => order.extend([PlPane::List, PlPane::Tracks].map(Stop::P)),
            Tab::Settings => order.push(Stop::P(PlPane::List)),
        }
        order.extend([Focus::Seekbar, Focus::Transport, Focus::Volume].map(Stop::F));
        let here = match (self.tab, self.focus) {
            (_, f @ (Focus::Search | Focus::Seekbar | Focus::Transport | Focus::Volume)) => Stop::F(f),
            (Tab::Local, f) => Stop::F(f),
            (Tab::Streaming, _) => Stop::P(self.sp_focus),
            (Tab::Browser, _) => Stop::P(self.br_focus),
            (Tab::Playlists, _) => Stop::P(self.pl_focus),
            (Tab::Settings, _) => Stop::P(PlPane::List),
        };
        let i = order.iter().position(|s| *s == here).unwrap_or(0) as i32;
        let n = order.len() as i32;
        match order[(i + if forward { 1 } else { -1 }).rem_euclid(n) as usize] {
            Stop::F(f) => self.set_focus(f),
            Stop::P(p) => {
                self.focus = Focus::Tracks;
                match self.tab {
                    Tab::Streaming => self.sp_focus = p,
                    Tab::Browser => self.br_focus = p,
                    Tab::Playlists => self.pl_focus = p,
                    _ => {}
                }
            }
        }
    }

    /// True when the keyboard is on the tab's own panes (not the search
    /// field or the bars at the bottom).
    pub fn panes_focused(&self) -> bool {
        !matches!(self.focus, Focus::Search | Focus::Seekbar | Focus::Transport | Focus::Volume)
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
            scroll_into_view(&mut self.queue_state, self.rects.queue.height as usize);
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
            scroll_into_view(&mut self.queue_state, self.rects.queue.height as usize);
        }
        self.notify("Removed from queue.");
    }

    /// Shift the selected queue item up (`delta < 0`) or down, carrying the
    /// selection and the now-playing marker with it.
    pub fn move_queue_item(&mut self, delta: i32) {
        let Some(i) = self.queue_state.selected() else {
            return;
        };
        let j = i as i64 + delta as i64;
        if i >= self.queue.len() || j < 0 || j as usize >= self.queue.len() {
            return;
        }
        let j = j as usize;
        self.queue.swap(i, j);
        self.now = match self.now {
            Some(n) if n == i => Some(j),
            Some(n) if n == j => Some(i),
            other => other,
        };
        self.queue_state.select(Some(j));
        scroll_into_view(&mut self.queue_state, self.rects.queue.height as usize);
    }

    /// Record the playing track as the persistent status line, clearing any
    /// transient message.
    fn set_now_playing(&mut self, track: &Track) {
        self.now_playing = format!("▶ {} — {}", track.title, track.track_artist);
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
        // A Spotify track without audio yet: fetch it first. Whatever is
        // playing keeps going until it arrives, then `poll_fetch` comes back
        // here and fades into it.
        if spotify::needs_fetch(&track.path) {
            self.now = Some(i);
            self.queue_state.select(Some(i));
            scroll_into_view(&mut self.queue_state, self.rects.queue.height as usize);
            self.pending_play = Some(track.path.clone());
            self.request_fetch(&track, false);
            self.now_playing = format!("⇣ Finding '{}' on YouTube…", track.title);
            self.status = self.now_playing.clone();
            self.notify_until = None;
            self.request_art(track.path.clone());
            return;
        }
        self.pending_play = None;
        // Crossfade into manually-picked tracks too (Enter, next/prev, IPC),
        // not just the automatic near-end handoff in `on_tick`. Skip it when
        // replaying the same track (e.g. "previous" restarting it) since fading
        // a track into itself just phases with its own tail. Compare by path,
        // not queue index: playing from the browser or a playlist replaces the
        // queue first, so a matching index there is usually a different track.
        let want_crossfade = self.audio.current_path() != Some(track.path.as_path())
            && self.audio.is_playing()
            && self.audio.crossfade_secs() > 0.0
            && (track.length_secs as f32) > self.audio.crossfade_secs();
        self.now = Some(i);
        self.queue_state.select(Some(i));
        scroll_into_view(&mut self.queue_state, self.rects.queue.height as usize);
        let result = if want_crossfade && self.audio.crossfade_to(&track.path, track.length_secs) {
            Ok(())
        } else {
            self.audio.play(&track.path, track.length_secs)
        };
        match result {
            Ok(()) => {
                self.set_now_playing(&track);
                self.request_art(track.path.clone());
                self.prefetch_next();
            }
            Err(e) => {
                self.status = format!("Error: {e}");
            }
        }
    }

    // --- spotify ---------------------------------------------------------

    /// The tabs shown in the tab bar, in order; number keys index into this.
    pub fn tabs(&self) -> Vec<Tab> {
        let mut tabs = vec![Tab::Local];
        if self.config.spotify.enabled {
            tabs.push(Tab::Streaming);
        }
        if self.config.browser.enabled {
            tabs.push(Tab::Browser);
        }
        if self.config.ui.show_playlists_tab {
            tabs.push(Tab::Playlists);
        }
        tabs.push(Tab::Settings);
        tabs
    }

    /// Switch tabs, swapping in that tab's own search text.
    pub fn set_tab(&mut self, tab: Tab) {
        if tab == self.tab {
            return;
        }
        if !self.config.ui.shared_search {
            let mine = std::mem::take(&mut self.search);
            self.searches.insert(self.tab, mine);
            self.search = self.searches.remove(&tab).unwrap_or_default();
            self.search_cursor = self.search.chars().count();
        }
        self.tab = tab;
    }

    /// The search text that applies to `tab`.
    pub fn search_for(&self, tab: Tab) -> String {
        if tab == self.tab || self.config.ui.shared_search {
            self.search.clone()
        } else {
            self.searches.get(&tab).cloned().unwrap_or_default()
        }
    }

    /// Settings: toggle one search shared by all tabs.
    pub fn toggle_shared_search(&mut self) {
        self.config.ui.shared_search = !self.config.ui.shared_search;
        self.searches.clear();
        let _ = self.config.save();
        self.refresh_views();
        self.notify(if self.config.ui.shared_search {
            "Search is now shared by all tabs."
        } else {
            "Each tab now keeps its own search."
        });
    }

    /// Switch to the `n`th (1-based) visible tab. False if there's no such tab.
    pub fn goto_tab(&mut self, n: usize) -> bool {
        match n.checked_sub(1).and_then(|i| self.tabs().get(i).copied()) {
            Some(tab) => {
                self.set_tab(tab);
                true
            }
            None => false,
        }
    }

    /// Load the Streaming library at startup (importing the CSV export on
    /// first run) and start reading linked playlists.
    pub fn load_spotify(&mut self) {
        self.sp_store = spotify::load_store();
        if self.sp_store.is_empty() {
            self.import_export();
        } else {
            // Re-read (not merge) the export for the details the library
            // file doesn't keep — ISRCs and release dates for tagging/filing.
            let _ = spotify::import(&self.config.spotify.csv);
        }
        let mut seen = HashSet::new();
        let fetched: Vec<Track> = self
            .sp_store
            .iter()
            .flat_map(|p| p.tracks.iter())
            .filter(|t| seen.insert(t.path.clone()) && std::fs::symlink_metadata(&t.path).is_ok())
            .cloned()
            .collect();
        let tx = self.fetch_tx.clone();
        std::thread::spawn(move || {
            spotify::prune_legacy_covers();
            let mut fetched = fetched;
            let known: HashSet<PathBuf> = fetched.iter().map(|t| t.path.clone()).collect();
            fetched.extend(spotify::orphan_files(&known));
            for t in spotify::organize_fetched(fetched) {
                let _ = tx.send(FetchMsg { path: t.path.clone(), result: Ok(t) });
            }
        });
        self.refresh_links();
        self.rebuild_streaming();
    }

    /// `R` in the Streaming tab: merge in anything new from the CSV export
    /// (nothing is ever removed) and re-read linked playlists.
    pub fn reimport_spotify(&mut self) {
        self.import_export();
        self.refresh_links();
        self.rebuild_streaming();
    }

    fn import_export(&mut self) {
        match spotify::import(&self.config.spotify.csv) {
            Ok(lists) => {
                self.spotify_error = None;
                let added = spotify::merge_import(&mut self.sp_store, lists);
                if added > 0 {
                    spotify::save_store(&self.sp_store);
                }
                self.notify(format!("Spotify export: {added} new tracks."));
            }
            Err(e) => self.spotify_error = Some(e.to_string()),
        }
    }

    /// Recombine the library and linked lists into the tab's lists, then
    /// re-sort and re-filter.
    fn rebuild_streaming(&mut self) {
        self.liked = self
            .sp_store
            .iter()
            .find(|p| p.name == spotify::LIKED)
            .map(|p| p.tracks.iter().map(|t| t.path.clone()).collect())
            .unwrap_or_default();
        // A linked playlist replaces the imported snapshot of the same name
        // (e.g. the export's frozen Release Radar) while it stays linked.
        let mut lists: Vec<Playlist> = self
            .sp_store
            .iter()
            .filter(|p| !self.is_shadowed(&p.name))
            .cloned()
            .collect();
        lists.extend(self.sp_linked.iter().map(|(_, p)| Playlist {
            name: format!("↻ {}", p.name),
            tracks: p.tracks.clone(),
        }));
        self.spotify_lists = lists;
        self.apply_sp_sort();
        self.refresh_sp_view();
    }

    /// True for an imported list hidden by a linked one of the same name.
    fn is_shadowed(&self, name: &str) -> bool {
        name != spotify::LIKED && self.sp_linked.iter().any(|(_, p)| p.name.eq_ignore_ascii_case(name))
    }

    fn refresh_links(&mut self) {
        for link in self.config.spotify.links.clone() {
            let tx = self.link_tx.clone();
            std::thread::spawn(move || {
                let result = spotify::fetch_linked(&link).map_err(|e| e.to_string());
                let _ = tx.send(LinkMsg { link, result });
            });
        }
    }

    fn poll_links(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.link_rx.try_recv() {
            changed = true;
            match msg.result {
                Ok(list) => {
                    match self.sp_linked.iter_mut().find(|(l, _)| *l == msg.link) {
                        Some(slot) => slot.1 = list,
                        None => self.sp_linked.push((msg.link, list)),
                    }
                    let order = &self.config.spotify.links;
                    self.sp_linked
                        .sort_by_key(|(l, _)| order.iter().position(|o| o == l).unwrap_or(usize::MAX));
                }
                Err(e) => self.notify(format!("Linked playlist failed: {e}")),
            }
        }
        if changed {
            self.rebuild_streaming();
        }
        changed
    }

    /// `u` in the Streaming tab: start pasting a Spotify playlist link.
    pub fn begin_link_input(&mut self) {
        self.link_input = Some(String::new());
    }

    pub fn commit_link_input(&mut self) {
        let Some(link) = self.link_input.take() else { return };
        let link = link.trim().to_string();
        if spotify::playlist_id(&link).is_none() {
            self.notify("That's not a Spotify playlist link.");
            return;
        }
        if !self.config.spotify.links.contains(&link) {
            self.config.spotify.links.push(link.clone());
            let _ = self.config.save();
        }
        let tx = self.link_tx.clone();
        std::thread::spawn(move || {
            let result = spotify::fetch_linked(&link).map_err(|e| e.to_string());
            let _ = tx.send(LinkMsg { link, result });
        });
        self.notify("Reading playlist…");
    }

    /// The Streaming-library list behind a displayed list name (None for
    /// linked lists, which are read-only).
    fn store_list_mut(&mut self, name: &str) -> Option<&mut Playlist> {
        self.sp_store.iter_mut().find(|p| p.name == name)
    }

    /// Append tracks to a library list (creating it), skipping ones already
    /// there. New tracks count as the list's most recent. Returns how many
    /// were added.
    fn add_to_store_list(&mut self, name: &str, tracks: &[Track]) -> usize {
        if self.store_list_mut(name).is_none() {
            let list = Playlist { name: name.to_string(), tracks: Vec::new() };
            if name == spotify::LIKED {
                self.sp_store.insert(0, list);
            } else {
                self.sp_store.push(list);
            }
        }
        let Some(list) = self.store_list_mut(name) else { return 0 };
        let mut next = list.tracks.iter().map(|t| t.id + 1).max().unwrap_or(0);
        let mut added = 0;
        for t in tracks {
            if list.tracks.iter().any(|x| x.path == t.path) {
                continue;
            }
            let mut t = t.clone();
            t.id = next;
            next += 1;
            list.tracks.push(t);
            added += 1;
        }
        spotify::save_store(&self.sp_store);
        self.rebuild_streaming();
        added
    }

    /// Like or unlike a track (Liked Songs in the Streaming library).
    fn toggle_like(&mut self, track: Track) {
        if !self.config.spotify.enabled {
            self.notify("Turn on the Spotify tab in Settings to keep likes.");
            return;
        }
        if self.liked.contains(&track.path) {
            self.confirm = Some(Confirm {
                title: " Remove from Liked Songs? ",
                subject: format!("{} — {}", track.title, track.track_artist),
                detail: "It stays in any other playlists it's in.".into(),
                yes: "Remove",
                no: "Keep",
                action: ConfirmAction::Unlike(track),
            });
        } else {
            self.add_to_store_list(spotify::LIKED, std::slice::from_ref(&track));
            self.notify(format!("♥ Liked '{}'.", track.title));
        }
    }

    /// The track an action applies to in the Streaming tab: the highlighted
    /// one, or the playing one when the list pane is focused.
    fn sp_target_track(&self) -> Option<Track> {
        match self.sp_focus {
            PlPane::Tracks => self
                .selected_sp_list()
                .zip(self.sp_track_state.selected())
                .and_then(|(p, i)| p.tracks.get(i).cloned()),
            PlPane::List => self.now.and_then(|i| self.queue.get(i).cloned()),
            PlPane::Queue => self.queue_selected_track(),
        }
    }

    /// The highlighted queue entry.
    fn queue_selected_track(&self) -> Option<Track> {
        self.queue_state.selected().and_then(|i| self.queue.get(i).cloned())
    }

    /// `f`: like/unlike the current track in the Streaming or Browser tab.
    pub fn like_current(&mut self) {
        let track = match self.tab {
            Tab::Browser => self.br_target_track(),
            _ => self.sp_target_track(),
        };
        if let Some(t) = track {
            self.toggle_like(t);
        }
    }

    /// `m`: open the add-to menu for the current track (or the whole list
    /// when the list pane is focused).
    pub fn open_add_menu(&mut self) {
        let tracks: Vec<Track> = match (self.tab, self.br_focus, self.sp_focus) {
            (Tab::Browser, PlPane::List, _) => self.br_current().map(|v| v.tracks()).unwrap_or_default(),
            (Tab::Browser, PlPane::Tracks, _) => self.br_target_track().into_iter().collect(),
            (_, _, PlPane::List) => self.selected_sp_list().map(|p| p.tracks.clone()).unwrap_or_default(),
            _ => self.sp_target_track().into_iter().collect(),
        };
        if tracks.is_empty() {
            return;
        }
        self.add_menu = Some(AddMenu { tracks, sel: 0, naming: None });
    }

    /// Choices in the add-to menu, with their labels.
    pub fn add_targets(&self) -> Vec<(String, AddTarget)> {
        let mut out = Vec::new();
        if self.config.spotify.enabled {
            for p in self.sp_store.iter().filter(|p| !self.is_shadowed(&p.name)) {
                out.push((format!("{}  ({})", p.name, p.tracks.len()), AddTarget::Store(p.name.clone())));
            }
            if !self.sp_store.iter().any(|p| p.name == spotify::LIKED) {
                out.insert(0, (spotify::LIKED.to_string(), AddTarget::Store(spotify::LIKED.into())));
            }
        }
        for (i, p) in self.playlists.iter().enumerate() {
            out.push((format!("{}  ({}) · local playlist", p.name, p.tracks.len()), AddTarget::Local(i)));
        }
        out.push(("＋ New playlist…".to_string(), AddTarget::New));
        out
    }

    pub fn add_menu_move(&mut self, delta: i32) {
        let n = self.add_targets().len() as i32;
        if let Some(m) = &mut self.add_menu {
            m.sel = (m.sel as i32 + delta).clamp(0, n - 1) as usize;
        }
    }

    pub fn add_menu_choose(&mut self) {
        let Some(menu) = &mut self.add_menu else { return };
        if let Some(name) = menu.naming.take() {
            let name = name.trim().to_string();
            let tracks = self.add_menu.take().map(|m| m.tracks).unwrap_or_default();
            if name.is_empty() {
                return;
            }
            if self.config.spotify.enabled {
                let n = self.add_to_store_list(&name, &tracks);
                self.notify(format!("Added {n} track(s) to new list '{name}'."));
            } else {
                self.playlists.push(Playlist { name: name.clone(), tracks });
                playlist::save(&self.playlists);
                self.clamp_pl_states();
                self.notify(format!("Created playlist '{name}'."));
            }
            return;
        }
        let sel = menu.sel;
        let Some((_, target)) = self.add_targets().into_iter().nth(sel) else { return };
        match target {
            AddTarget::New => {
                if let Some(m) = &mut self.add_menu {
                    m.naming = Some(String::new());
                }
            }
            AddTarget::Store(name) => {
                let tracks = self.add_menu.take().map(|m| m.tracks).unwrap_or_default();
                let n = self.add_to_store_list(&name, &tracks);
                self.notify(format!("Added {n} track(s) to '{name}'."));
            }
            AddTarget::Local(i) => {
                let tracks = self.add_menu.take().map(|m| m.tracks).unwrap_or_default();
                if let Some(pl) = self.playlists.get_mut(i) {
                    let n = tracks.len();
                    pl.tracks.extend(tracks);
                    let name = pl.name.clone();
                    playlist::save(&self.playlists);
                    self.clamp_pl_states();
                    self.notify(format!("Added {n} track(s) to '{name}'."));
                }
            }
        }
    }

    /// `d` in the Streaming tab: remove the highlighted track from its list,
    /// or unlink a linked playlist when the list pane is focused.
    fn unlike(&mut self, track: &Track) {
        if let Some(l) = self.store_list_mut(spotify::LIKED) {
            l.tracks.retain(|t| t.path != track.path);
        }
        spotify::save_store(&self.sp_store);
        self.rebuild_streaming();
        self.notify(format!("Removed '{}' from Liked Songs.", track.title));
    }

    /// `d` in the Spotify tab: confirm, then unlink the list or remove the song.
    pub fn sp_delete(&mut self) {
        let Some(list) = self.selected_sp_list().cloned() else { return };
        let linked = list.name.starts_with("↻ ");
        let (title, subject, detail, yes) = match (self.sp_focus, linked) {
            (PlPane::List, true) => (
                " Unlink playlist? ",
                list.name.clone(),
                "It stops refreshing and leaves the sidebar. You can link it again with u.".to_string(),
                "Unlink",
            ),
            (PlPane::Tracks, false) => {
                let Some(t) = self.sp_target_track() else { return };
                (" Remove song? ", format!("{} — {}", t.title, t.track_artist), format!("From '{}'.", list.name), "Remove")
            }
            // Nothing to remove; let the action explain why.
            _ => return self.sp_delete_now(),
        };
        self.confirm = Some(Confirm { title, subject, detail, yes, no: "Cancel", action: ConfirmAction::SpDelete });
    }

    fn sp_delete_now(&mut self) {
        let Some(list) = self.selected_sp_list().cloned() else { return };
        let linked = list.name.strip_prefix("↻ ").map(str::to_string);
        match (self.sp_focus, linked) {
            (PlPane::List, Some(name)) => {
                if let Some(i) = self.sp_linked.iter().position(|(_, p)| p.name == name) {
                    let (link, _) = self.sp_linked.remove(i);
                    self.config.spotify.links.retain(|l| *l != link);
                    let _ = self.config.save();
                    self.rebuild_streaming();
                    self.notify(format!("Unlinked '{name}'."));
                }
            }
            (PlPane::List, None) => self.notify("Only linked (↻) lists can be removed."),
            (PlPane::Queue, _) => self.remove_from_queue(),
            (PlPane::Tracks, Some(_)) => self.notify("Linked lists are read-only."),
            (PlPane::Tracks, None) => {
                let Some(track) = self.sp_target_track() else { return };
                if let Some(l) = self.store_list_mut(&list.name) {
                    l.tracks.retain(|t| t.path != track.path);
                }
                spotify::save_store(&self.sp_store);
                self.rebuild_streaming();
                self.notify(format!("Removed '{}' from '{}'.", track.title, list.name));
            }
        }
    }

    pub fn toggle_spotify(&mut self) {
        self.config.spotify.enabled = !self.config.spotify.enabled;
        let _ = self.config.save();
        if self.config.spotify.enabled {
            self.load_spotify();
            if let Some(e) = &self.spotify_error {
                self.notify(format!("Spotify tab on — {e}"));
            }
        } else {
            if self.tab == Tab::Streaming {
                self.set_tab(Tab::Settings);
            }
            self.notify("Spotify tab off.");
        }
    }

    pub fn toggle_browser(&mut self) {
        self.config.browser.enabled = !self.config.browser.enabled;
        let _ = self.config.save();
        if !self.config.browser.enabled && self.tab == Tab::Browser {
            self.set_tab(Tab::Settings);
        }
        self.notify(format!("Explore tab {}.", if self.config.browser.enabled { "on" } else { "off" }));
    }

    pub fn toggle_playlists_tab(&mut self) {
        self.config.ui.show_playlists_tab = !self.config.ui.show_playlists_tab;
        let _ = self.config.save();
        if !self.config.ui.show_playlists_tab && self.tab == Tab::Playlists {
            self.set_tab(Tab::Settings);
        }
        self.notify(format!(
            "Local Playlists tab {}.",
            if self.config.ui.show_playlists_tab { "on" } else { "off" }
        ));
    }

    // --- settings --------------------------------------------------------

    pub fn settings_rows(&self) -> Vec<SettingRow> {
        use SettingRow::*;
        let mut rows = vec![
            Header("Local library"),
            MusicDir,
            Rescan,
            LocalArtDir,
            Header("Playback"),
            Gapless,
            KeepGoing,
            Header("Appearance"),
            Thumbnails,
            CacheAllArt,
            SortArticles,
            BigIcons,
            Header("Tabs"),
            TabSpotify,
            TabExplore,
            TabPlaylists,
            SharedSearch,
            Header("Spotify"),
            SpotifyCsv,
        ];
        if self.config.spotify.enabled {
            rows.extend([SpotifyReimport, SpotifyLinks]);
        }
        rows.extend([DownloadsDir, SpotifyCoversDir, LibraryFile, Header("Config"), ConfigFile]);
        rows
    }

    pub fn selected_setting(&self) -> Option<SettingRow> {
        self.settings_state.selected().and_then(|i| self.settings_rows().get(i).copied())
    }

    /// Move the Settings selection, skipping headers.
    pub fn settings_move(&mut self, delta: i32) {
        let rows = self.settings_rows();
        let mut i = self.settings_state.selected().unwrap_or(0) as i32;
        let step = delta.signum();
        for _ in 0..delta.unsigned_abs() {
            let mut j = i + step;
            while j >= 0 && (j as usize) < rows.len() && !rows[j as usize].selectable() {
                j += step;
            }
            if j < 0 || j as usize >= rows.len() {
                break;
            }
            i = j;
        }
        self.settings_state.select(Some(i as usize));
        scroll_into_view(&mut self.settings_state, self.rects.settings.height as usize);
    }

    /// Enter/Space on a Settings row: toggle, edit, or run it.
    pub fn settings_activate(&mut self) {
        use SettingRow::*;
        let Some(row) = self.selected_setting() else { return };
        match row {
            Header(_) => {}
            MusicDir => self.begin_edit_path(PathField::MusicDir),
            SpotifyCsv => self.begin_edit_path(PathField::SpotifyCsv),
            Rescan => self.start_scan(),
            Gapless => self.adjust_gapless(0.5),
            KeepGoing => self.toggle_keep_going(),
            CacheAllArt => self.toggle_cache_all_art(),
            SortArticles => self.toggle_sort_articles(),
            Thumbnails => self.toggle_album_thumbnails(),
            BigIcons => self.toggle_big_icons(),
            TabSpotify => self.toggle_spotify(),
            TabExplore => self.toggle_browser(),
            TabPlaylists => self.toggle_playlists_tab(),
            SharedSearch => self.toggle_shared_search(),
            SpotifyReimport => self.reimport_spotify(),
            SpotifyLinks => self.begin_link_input(),
            DownloadsDir => open_in_file_manager(&spotify::music_root()),
            SpotifyCoversDir => open_in_file_manager(&crate::config::spotify_cache_dir().join("covers")),
            LocalArtDir => open_in_file_manager(&crate::config::art_cache_dir()),
            LibraryFile | ConfigFile => {
                let path = if row == LibraryFile {
                    crate::config::streaming_store_path()
                } else {
                    crate::config::config_path()
                };
                if let Some(dir) = path.parent() {
                    open_in_file_manager(dir);
                }
            }
        }
        // Toggling a tab can add/remove rows; keep the cursor on this row.
        if let Some(i) = self.settings_rows().iter().position(|r| *r == row) {
            self.settings_state.select(Some(i));
        }
    }

    /// ←/→ on a Settings row: step the gapless fade, or toggle a switch.
    pub fn settings_adjust(&mut self, delta: i32) {
        match self.selected_setting() {
            Some(SettingRow::Gapless) => self.adjust_gapless(0.5 * delta as f64),
            Some(SettingRow::KeepGoing) => self.toggle_keep_going(),
            Some(
                SettingRow::CacheAllArt
                | SettingRow::SortArticles
                | SettingRow::Thumbnails
                | SettingRow::BigIcons
                | SettingRow::TabSpotify
                | SettingRow::TabExplore
                | SettingRow::TabPlaylists,
            ) => self.settings_activate(),
            _ => {}
        }
    }

    fn refresh_cache_stats(&mut self) {
        if self.cache_stats_at.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.cache_stats_at = Some(Instant::now());
        self.cache_stats = Some((
            dir_stats(&spotify::music_root()),
            dir_stats(&crate::config::spotify_cache_dir().join("covers")),
            dir_stats(&crate::config::art_cache_dir()),
        ));
    }

    // --- browser ---------------------------------------------------------

    pub fn br_section_count(&self) -> usize {
        self.br_results.len() + self.br_radios.len() + self.br_home.len()
    }

    pub fn br_section(&self, i: usize) -> Option<&deezer::View> {
        let r = self.br_results.len();
        let rr = r + self.br_radios.len();
        match i {
            i if i < r => self.br_results.get(i),
            i if i < rr => self.br_radios.get(i - r),
            i => self.br_home.get(i - rr),
        }
    }

    /// What the right pane shows: the opened item on top of the stack, else
    /// the selected section.
    pub fn br_current(&self) -> Option<&deezer::View> {
        self.br_stack
            .last()
            .or_else(|| self.br_sec_state.selected().and_then(|i| self.br_section(i)))
    }

    fn br_target_track(&self) -> Option<Track> {
        if self.br_focus == PlPane::Queue {
            return self.queue_selected_track();
        }
        let view = self.br_current()?;
        match view.items.get(self.br_item_state.selected()?)? {
            deezer::Item::Track(t) => Some(t.clone()),
            _ => None,
        }
    }

    /// Load the landing sections the first time the tab is shown.
    fn ensure_browser_home(&mut self) {
        if self.br_home_requested {
            return;
        }
        self.br_home_requested = true;
        self.br_loading = Some("charts".into());
        let tx = self.br_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match deezer::home() {
                Ok(v) => BrMsg::Home(v),
                Err(e) => BrMsg::Error(e.to_string()),
            });
        });
        self.request_for_you();
    }

    /// Build "For you" from the most-liked artists in Liked Songs.
    fn request_for_you(&mut self) {
        let Some(liked) = self.sp_store.iter().find(|p| p.name == spotify::LIKED).cloned() else { return };
        let mut counts: HashMap<String, usize> = HashMap::new();
        for t in &liked.tracks {
            let main = t.track_artist.split(", ").next().unwrap_or(&t.track_artist).to_string();
            *counts.entry(main).or_default() += 1;
        }
        let mut ranked: Vec<(String, usize)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        // The top artists, plus a few picked at random from the rest so the
        // mix isn't the same every launch.
        let mut seeds: Vec<String> = ranked.iter().take(5).map(|(a, _)| a.clone()).collect();
        let rest: Vec<String> = ranked.iter().skip(5).map(|(a, _)| a.clone()).collect();
        for _ in 0..3 {
            if rest.is_empty() {
                break;
            }
            let pick = rest[(self.next_rand() % rest.len() as u64) as usize].clone();
            if !seeds.contains(&pick) {
                seeds.push(pick);
            }
        }
        let known: HashSet<(String, String)> = liked
            .tracks
            .iter()
            .map(|t| (t.track_artist.to_lowercase(), t.title.to_lowercase()))
            .collect();
        let tx = self.br_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match deezer::for_you(&seeds, &known) {
                Ok(v) => BrMsg::ForYou(v),
                Err(e) => BrMsg::Error(format!("For you: {e}")),
            });
        });
    }

    /// Fire a Deezer search once the search text has been still for a moment.
    fn br_maybe_search(&mut self) {
        let q = self.search.trim().to_string();
        if q != self.br_seen.0 {
            self.br_seen = (q, Instant::now());
            return;
        }
        if q == self.br_query || self.br_seen.1.elapsed() < Duration::from_millis(600) {
            return;
        }
        self.br_search_now();
    }

    /// Run the Browser search for the current search text immediately.
    pub fn br_search_now(&mut self) {
        let q = self.search.trim().to_string();
        self.br_seen = (q.clone(), Instant::now());
        self.br_query = q.clone();
        if q.is_empty() {
            self.br_results.clear();
            self.br_stack.clear();
            self.br_sec_state.select(Some(0));
            self.br_item_state = ListState::default();
            self.br_item_state.select(Some(0));
            return;
        }
        self.br_loading = Some(format!("“{q}”"));
        let tx = self.br_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match deezer::search(&q) {
                Ok(v) => BrMsg::Results(q, v),
                Err(e) => BrMsg::Error(e.to_string()),
            });
        });
    }

    fn poll_browser(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.br_rx.try_recv() {
            changed = true;
            match msg {
                BrMsg::Home(views) => {
                    self.br_loading = None;
                    let for_you: Vec<deezer::View> =
                        self.br_home.drain(..).filter(|v| v.title == "For you").collect();
                    self.br_home = for_you;
                    self.br_home.extend(views);
                }
                BrMsg::ForYou(view) => {
                    self.br_home.retain(|v| v.title != "For you");
                    self.br_home.insert(0, view);
                }
                BrMsg::Results(q, views) => {
                    if q != self.br_query {
                        continue;
                    }
                    self.br_loading = None;
                    self.br_results = views;
                    self.br_stack.clear();
                    self.br_sec_state = ListState::default();
                    self.br_sec_state.select(Some(0));
                    self.br_item_state = ListState::default();
                    self.br_item_state.select(Some(0));
                    if self.br_results.is_empty() {
                        self.notify(format!("No results for “{q}”."));
                    }
                }
                BrMsg::Opened(view) => {
                    self.br_loading = None;
                    self.br_stack.push(view);
                    self.br_item_state = ListState::default();
                    self.br_item_state.select(Some(0));
                    self.br_focus = PlPane::Tracks;
                }
                BrMsg::Radio { view, extend } => self.on_radio(view, extend),
                BrMsg::RadioFailed { title, error, extend } => self.on_radio_failed(title, error, extend),
                BrMsg::Error(e) => {
                    self.br_loading = None;
                    self.radio_extending = false;
                    self.notify(e);
                }
            }
        }
        if changed {
            let sections = self.br_section_count();
            clamp(&mut self.br_sec_state, sections);
            let n = self.br_current().map(|v| v.items.len()).unwrap_or(0);
            clamp(&mut self.br_item_state, n);
        }
        changed
    }

    pub fn br_move(&mut self, delta: i32) {
        let row_h = self.sp_row_h();
        match self.br_focus {
            PlPane::List => {
                let before = self.br_sec_state.selected();
                let sections = self.br_section_count();
                move_state(&mut self.br_sec_state, sections, delta);
                scroll_into_view(&mut self.br_sec_state, self.rects.br_sections.height as usize);
                if self.br_sec_state.selected() != before || delta == 0 {
                    self.br_stack.clear();
                    self.br_item_state = ListState::default();
                    self.br_item_state.select(Some(0));
                }
            }
            PlPane::Tracks => {
                let n = self.br_current().map(|v| v.items.len()).unwrap_or(0);
                move_state(&mut self.br_item_state, n, delta);
                scroll_into_view(&mut self.br_item_state, (self.rects.br_items.height / row_h) as usize);
            }
            PlPane::Queue => self.queue_move(delta),
        }
    }

    /// True when the queue pane has the keyboard focus, in any tab.
    pub fn queue_focused(&self) -> bool {
        match self.tab {
            Tab::Local => self.focus == Focus::Queue,
            Tab::Streaming => self.sp_focus == PlPane::Queue,
            Tab::Browser => self.br_focus == PlPane::Queue,
            _ => false,
        }
    }

    /// Move the queue selection (Spotify/Explore queue pane).
    pub fn queue_move(&mut self, delta: i32) {
        move_state(&mut self.queue_state, self.queue.len(), delta);
        scroll_into_view(&mut self.queue_state, self.rects.queue.height as usize);
    }

    /// Play the highlighted queue entry.
    pub fn queue_play_selected(&mut self) {
        if let Some(i) = self.queue_state.selected().filter(|&i| i < self.queue.len()) {
            self.play_index(i);
        }
    }

    /// `Q`: show or hide the queue (remembered).
    pub fn toggle_queue(&mut self) {
        self.config.ui.show_queue = !self.config.ui.show_queue;
        let _ = self.config.save();
        if !self.config.ui.show_queue {
            if self.focus == Focus::Queue {
                self.focus = Focus::Tracks;
            }
            if self.sp_focus == PlPane::Queue {
                self.sp_focus = PlPane::Tracks;
            }
            if self.br_focus == PlPane::Queue {
                self.br_focus = PlPane::Tracks;
            }
        }
        self.notify(if self.config.ui.show_queue { "Queue shown (Q hides it)." } else { "Queue hidden (Q shows it)." });
    }

    pub fn br_set_pane(&mut self, pane: PlPane) {
        if !self.panes_focused() {
            self.focus = Focus::Tracks;
        }
        self.br_focus = pane;
    }

    /// Enter: from a section, step into its items; on a track, play the view
    /// from it; on an album/artist/playlist, open it.
    pub fn br_activate(&mut self) {
        if self.br_focus == PlPane::List {
            self.br_focus = PlPane::Tracks;
            return;
        }
        let Some(view) = self.br_current().cloned() else { return };
        let Some(item) = self.br_item_state.selected().and_then(|i| view.items.get(i)).cloned() else {
            return;
        };
        let open = match item {
            deezer::Item::Track(t) => {
                let tracks = view.tracks();
                let start = tracks.iter().position(|x| x.path == t.path).unwrap_or(0);
                self.play_tracks(tracks, start);
                return;
            }
            deezer::Item::Album { id, title, .. } => deezer::Open::Album(id, title),
            deezer::Item::Artist { id, name, .. } => deezer::Open::Artist(id, name),
            deezer::Item::Playlist { id, title, .. } => deezer::Open::Playlist(id, title),
        };
        self.br_loading = Some("…".into());
        let tx = self.br_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match deezer::open(&open) {
                Ok(v) => BrMsg::Opened(v),
                Err(e) => BrMsg::Error(e.to_string()),
            });
        });
    }

    /// Backspace/Esc: close the opened album/artist/playlist. False if
    /// nothing was open.
    pub fn br_back(&mut self) -> bool {
        if self.br_stack.pop().is_none() {
            return false;
        }
        self.br_item_state = ListState::default();
        self.br_item_state.select(Some(0));
        true
    }

    /// `a`: queue the highlighted track, or the whole view from the section pane.
    pub fn br_add_to_queue(&mut self) {
        let tracks: Vec<Track> = match self.br_focus {
            PlPane::List => self.br_current().map(|v| v.tracks()).unwrap_or_default(),
            PlPane::Tracks => self.br_target_track().into_iter().collect(),
            PlPane::Queue => return,
        };
        if tracks.is_empty() {
            return;
        }
        let n = tracks.len();
        self.queue.extend(tracks);
        if self.queue_state.selected().is_none() {
            self.queue_state.select(Some(0));
        }
        self.notify(format!("Added {n} track(s) to queue."));
    }

    // --- song radio ------------------------------------------------------

    /// The track `g` builds a radio from: the highlighted one in the current
    /// tab, else the one playing.
    fn radio_target(&self) -> Option<Track> {
        let highlighted = match self.tab {
            Tab::Local if self.focus == Focus::Tracks => {
                self.track_state.selected().and_then(|i| self.view_tracks.get(i).cloned())
            }
            Tab::Local if self.focus == Focus::Queue => self.queue_selected_track(),
            Tab::Streaming if self.sp_focus != PlPane::List => self.sp_target_track(),
            Tab::Browser if self.br_focus != PlPane::List => self.br_target_track(),
            _ => None,
        };
        highlighted.or_else(|| self.now.and_then(|i| self.queue.get(i).cloned()))
    }

    /// `g`: open the "Go to…" menu for the target track.
    pub fn open_goto_menu(&mut self) {
        match self.radio_target() {
            Some(track) => self.goto_menu = Some(GotoMenu { track, sel: 0 }),
            None => self.notify("Highlight a song first."),
        }
    }

    /// The menu's entries: key, label, action.
    pub fn goto_entries(&self) -> Vec<(char, String, GotoKind)> {
        let Some(m) = &self.goto_menu else { return Vec::new() };
        let artist = m.track.track_artist.split(", ").next().unwrap_or(&m.track.track_artist);
        let mut out = vec![
            ('r', "Song radio  (similar songs)".to_string(), GotoKind::Radio),
            ('a', format!("Artist  · {artist}"), GotoKind::Artist),
        ];
        if !m.track.album.is_empty() {
            out.push(('l', format!("Album  · {}", m.track.album), GotoKind::Album));
        }
        let on = if self.config.playback.keep_going { "on" } else { "off" };
        out.push(('c', format!("Keep going when the queue ends: {on}"), GotoKind::KeepGoing));
        out
    }

    pub fn goto_move(&mut self, delta: i32) {
        let n = self.goto_entries().len();
        if let Some(m) = &mut self.goto_menu {
            m.sel = (m.sel as i32 + delta).rem_euclid(n.max(1) as i32) as usize;
        }
    }

    /// Run a "Go to…" entry: the highlighted one, or the one for `key`.
    pub fn goto_choose(&mut self, key: Option<char>) {
        let entries = self.goto_entries();
        let Some(m) = self.goto_menu.take() else { return };
        let pick = match key {
            Some(k) => entries.iter().find(|e| e.0 == k),
            None => entries.get(m.sel),
        };
        let Some(&(_, _, kind)) = pick else {
            self.goto_menu = Some(m);
            return;
        };
        match kind {
            GotoKind::Radio => self.start_radio(m.track),
            GotoKind::Artist => self.goto_page(m.track, false),
            GotoKind::Album => self.goto_page(m.track, true),
            GotoKind::KeepGoing => self.toggle_keep_going(),
        }
    }

    /// Open the track's artist or album page in Explore.
    fn goto_page(&mut self, track: Track, album: bool) {
        if !self.config.browser.enabled {
            self.notify("Turn on the Explore tab in Settings to open artist and album pages.");
            return;
        }
        let what = if album { track.album.clone() } else { track.track_artist.clone() };
        self.br_loading = Some(format!("“{what}”"));
        self.set_tab(Tab::Browser);
        let tx = self.br_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(match deezer::locate(&track, album).and_then(|o| deezer::open(&o)) {
                Ok(v) => BrMsg::Opened(v),
                Err(e) => BrMsg::Error(e.to_string()),
            });
        });
    }

    /// Build a radio of similar songs from `seed`. It appears in Explore's
    /// sidebar right away (marked as building) and fills in when ready.
    fn start_radio(&mut self, seed: Track) {
        if !self.config.browser.enabled {
            self.notify(format!("Building a radio from '{}' — it'll be added to the queue…", seed.title));
            self.spawn_radio(seed, false);
            return;
        }
        let title = format!("Radio: {}", seed.title);
        self.br_radios.retain(|v| v.title != title);
        self.br_radios.insert(0, deezer::View { title: title.clone(), items: Vec::new() });
        self.br_radios.truncate(5);
        self.br_radio_building = Some(title);
        self.br_stack.clear();
        self.br_sec_state.select(Some(self.br_results.len()));
        self.br_item_state = ListState::default();
        self.br_item_state.select(Some(0));
        self.br_focus = PlPane::Tracks;
        self.set_tab(Tab::Browser);
        self.spawn_radio(seed, false);
    }

    fn spawn_radio(&mut self, seed: Track, extend: bool) {
        let tx = self.br_tx.clone();
        std::thread::spawn(move || {
            let title = format!("Radio: {}", seed.title);
            let _ = tx.send(match deezer::radio(&seed, 50) {
                Ok(view) => BrMsg::Radio { view, extend },
                Err(e) => BrMsg::RadioFailed { title, error: e.to_string(), extend },
            });
        });
    }

    fn on_radio_failed(&mut self, title: String, error: String, extend: bool) {
        if extend {
            self.radio_extending = false;
        } else {
            self.br_radios.retain(|v| v.title != title);
            if self.br_radio_building.as_deref() == Some(title.as_str()) {
                self.br_radio_building = None;
            }
        }
        self.notify(format!("Radio: {error}"));
    }

    /// Swap radio picks the user already owns for their local files.
    fn prefer_local(&self, view: &mut deezer::View) -> usize {
        let key = |artist: &str, title: &str| {
            let main = artist.split([',', '&']).next().unwrap_or(artist).trim().to_lowercase();
            (main, title.trim().to_lowercase())
        };
        let local: HashMap<(String, String), &Track> =
            self.library.tracks.iter().map(|t| (key(&t.track_artist, &t.title), t)).collect();
        let mut n = 0;
        for item in &mut view.items {
            if let deezer::Item::Track(t) = item
                && let Some(l) = local.get(&key(&t.track_artist, &t.title))
            {
                *t = (*l).clone();
                n += 1;
            }
        }
        n
    }

    fn on_radio(&mut self, mut view: deezer::View, extend: bool) {
        let local = self.prefer_local(&mut view);
        if extend {
            self.radio_extending = false;
            let have: HashSet<(String, String)> = self
                .queue
                .iter()
                .map(|t| (t.track_artist.to_lowercase(), t.title.to_lowercase()))
                .collect();
            let paths: HashSet<PathBuf> = self.queue.iter().map(|t| t.path.clone()).collect();
            let add: Vec<Track> = view
                .tracks()
                .into_iter()
                .filter(|t| !paths.contains(&t.path) && !have.contains(&(t.track_artist.to_lowercase(), t.title.to_lowercase())))
                .take(25)
                .collect();
            if !add.is_empty() {
                let n = add.len();
                self.queue.extend(add);
                self.notify(format!("Keep going: queued {n} similar songs."));
            }
            return;
        }
        let n = view.items.len();
        let note = if local > 0 { format!(" ({local} from your library)") } else { String::new() };
        if self.config.browser.enabled {
            if self.br_radio_building.as_deref() == Some(view.title.as_str()) {
                self.br_radio_building = None;
            }
            let showing = self.br_stack.is_empty() && self.br_current().is_some_and(|v| v.title == view.title);
            match self.br_radios.iter_mut().find(|v| v.title == view.title) {
                Some(slot) => *slot = view,
                None => {
                    self.br_radios.insert(0, view);
                    self.br_radios.truncate(5);
                }
            }
            if showing {
                self.br_item_state = ListState::default();
                self.br_item_state.select(Some(0));
            }
            self.notify(format!("Radio ready: {n} similar songs{note}."));
        } else {
            self.queue.extend(view.tracks());
            self.notify(format!("Queued {n} similar songs{note}."));
        }
    }

    /// Keep-going mode: when the queue is down to its last track, build a
    /// radio from what's playing and append it.
    fn maybe_keep_going(&mut self) {
        if !self.config.playback.keep_going || self.radio_extending || self.shuffle || self.repeat != RepeatMode::Off {
            return;
        }
        let Some(i) = self.now else { return };
        let Some(cur) = self.queue.get(i) else { return };
        if i + 1 < self.queue.len() || self.radio_seed.as_ref() == Some(&cur.path) {
            return;
        }
        let seed = cur.clone();
        self.radio_seed = Some(seed.path.clone());
        self.radio_extending = true;
        self.spawn_radio(seed, true);
    }

    /// `G` / Settings: toggle keep-going mode.
    pub fn toggle_keep_going(&mut self) {
        self.config.playback.keep_going = !self.config.playback.keep_going;
        let _ = self.config.save();
        self.notify(if self.config.playback.keep_going {
            "Keep going on: similar songs are queued when the queue runs out."
        } else {
            "Keep going off."
        });
    }

    /// `S` in Explore: save the open list (e.g. a radio) as a Spotify-tab
    /// playlist.
    pub fn br_save_view(&mut self) {
        if !self.config.spotify.enabled {
            self.notify("Turn on the Spotify tab in Settings to save playlists.");
            return;
        }
        let Some(view) = self.br_current().cloned() else { return };
        let tracks = view.tracks();
        if tracks.is_empty() {
            return;
        }
        let added = self.add_to_store_list(&view.title, &tracks);
        self.notify(format!("Saved '{}' ({added} songs) to your Spotify-tab library.", view.title));
    }

    /// `x` in the Browser tab: reject the highlighted track's YouTube match.
    pub fn br_reject(&mut self) {
        if let Some(t) = self.br_target_track() {
            self.ask_reject(t);
        }
    }

    pub fn selected_sp_list(&self) -> Option<&Playlist> {
        self.sp_list_state
            .selected()
            .and_then(|i| self.sp_view.get(i))
    }

    /// Text rows per Spotify-tab row: two (cover + two text lines) when
    /// thumbnails are on.
    pub fn sp_row_h(&self) -> u16 {
        if self.show_album_thumbnails { Self::ALBUM_ROW_HEIGHT } else { 1 }
    }

    /// Thumbnail cache key for a Spotify track's cover: its album, so every
    /// track of an album shares one decoded thumbnail.
    pub fn sp_track_thumb_key(t: &Track) -> AlbumRef {
        AlbumRef { album_artist: t.album_artist.clone(), title: t.album.clone(), year: t.year }
    }

    /// Thumbnail cache key for a Spotify list's cover mosaic. The leading
    /// control character keeps it from colliding with a real album.
    pub fn sp_list_thumb_key(p: &Playlist) -> AlbumRef {
        AlbumRef { album_artist: "\u{1}list".into(), title: p.name.clone(), year: None }
    }

    fn apply_sp_sort(&mut self) {
        let sort = self.sp_sort;
        let lower = |s: &str| s.to_lowercase();
        for list in &mut self.spotify_lists {
            match sort {
                SpSort::Recent => list.tracks.sort_by_key(|t| std::cmp::Reverse(t.id)),
                SpSort::Oldest => list.tracks.sort_by_key(|t| t.id),
                SpSort::Title => list.tracks.sort_by_key(|t| (lower(&t.title), t.id)),
                SpSort::Artist => {
                    list.tracks.sort_by_key(|t| (lower(&t.track_artist), lower(&t.album), t.id))
                }
                SpSort::Album => list.tracks.sort_by_key(|t| (lower(&t.album), t.track_no, t.id)),
            }
        }
    }

    /// `o` in the Spotify tab: cycle the track order of every list.
    pub fn sp_cycle_sort(&mut self) {
        self.sp_sort = self.sp_sort.next();
        self.apply_sp_sort();
        self.refresh_sp_view();
        let has = self.selected_sp_list().is_some_and(|p| !p.tracks.is_empty());
        self.sp_track_state = ListState::default();
        self.sp_track_state.select(has.then_some(0));
        self.notify(format!("Sort: {}", self.sp_sort.label()));
    }

    /// Keyboard resize (Alt+] / Alt+[) of the Spotify tab's list pane.
    pub fn sp_resize(&mut self, wider: bool) {
        let d = if wider { 4 } else { -4 };
        self.sp_pct = (self.sp_pct as i32 + d).clamp(12, 70) as u16;
        self.persist_sp_pct();
    }

    /// Drag the Spotify tab's pane divider to mouse x (saved on release).
    pub fn drag_sp_divider(&mut self, x: u16) {
        let body = self.rects.sp_body;
        if body.width == 0 {
            return;
        }
        let rel = (x as i32 - body.x as i32).clamp(0, body.width as i32) as f64;
        self.sp_pct = ((rel / body.width as f64 * 100.0).round() as i32).clamp(12, 70) as u16;
    }

    pub fn persist_sp_pct(&mut self) {
        self.config.layout.spotify_pct = self.sp_pct;
        let _ = self.config.save();
    }

    /// Background cover loads for Spotify-tab rows in view: one cover per
    /// track row, and a 2×2 mosaic of distinct album covers per list (like
    /// Spotify's playlist art). Covers may need a network fetch, so only a
    /// few run at once.
    fn request_visible_sp_thumbs(&mut self) {
        if !self.show_album_thumbnails || !self.art.available() {
            return;
        }
        let row_h = self.sp_row_h();
        let mut jobs: Vec<(AlbumRef, Vec<PathBuf>)> = Vec::new();

        let visible = (self.rects.sp_list.height / row_h) as usize;
        let offset = self.sp_list_state.offset();
        for list in thumb_window(offset, visible, self.sp_view.len()).map(|i| &self.sp_view[i]) {
            let mut albums = HashSet::new();
            let paths: Vec<PathBuf> = list
                .tracks
                .iter()
                .filter(|t| albums.insert(Self::sp_track_thumb_key(t)))
                .take(4)
                .map(|t| t.path.clone())
                .collect();
            jobs.push((Self::sp_list_thumb_key(list), paths));
        }
        let visible = (self.rects.sp_tracks.height / row_h) as usize;
        let offset = self.sp_track_state.offset();
        if let Some(list) = self.selected_sp_list() {
            for t in thumb_window(offset, visible, list.tracks.len()).map(|i| &list.tracks[i]) {
                jobs.push((Self::sp_track_thumb_key(t), vec![t.path.clone()]));
            }
        }

        let jobs = jobs
            .into_iter()
            .map(|(k, paths)| (k, paths.into_iter().map(ThumbSrc::Track).collect()))
            .collect();
        self.spawn_thumb_jobs(jobs);
    }

    /// Thumbnail cache key for a Browser album/artist/playlist image URL.
    pub fn url_thumb_key(url: &str) -> AlbumRef {
        AlbumRef { album_artist: "\u{2}url".into(), title: url.to_string(), year: None }
    }

    /// The thumbnail key and image source for one Browser row.
    pub fn br_item_thumb(item: &deezer::Item) -> (AlbumRef, ThumbSrc) {
        match item {
            deezer::Item::Track(t) => (Self::sp_track_thumb_key(t), ThumbSrc::Track(t.path.clone())),
            deezer::Item::Album { cover: u, .. }
            | deezer::Item::Artist { picture: u, .. }
            | deezer::Item::Playlist { cover: u, .. } => (Self::url_thumb_key(u), ThumbSrc::Url(u.clone())),
        }
    }

    /// Prefetch every cover of the list open in the Spotify/Explore tab
    /// (plus the Spotify list mosaics) to disk, once per list.
    fn prefetch_open_list_covers(&mut self) {
        let (key, srcs): (String, Vec<spotify::CoverSrc>) = match self.tab {
            Tab::Streaming => {
                let Some(list) = self.selected_sp_list() else { return };
                let key = format!("sp\u{0}{}\u{0}{}", list.name, list.tracks.len());
                if key == self.cover_job {
                    return;
                }
                let mut srcs: Vec<spotify::CoverSrc> =
                    list.tracks.iter().map(|t| spotify::CoverSrc::Track(t.path.clone())).collect();
                for l in &self.sp_view {
                    let mut albums = HashSet::new();
                    srcs.extend(
                        l.tracks
                            .iter()
                            .filter(|t| albums.insert(Self::sp_track_thumb_key(t)))
                            .take(4)
                            .map(|t| spotify::CoverSrc::Track(t.path.clone())),
                    );
                }
                (key, srcs)
            }
            Tab::Browser => {
                let Some(view) = self.br_current() else { return };
                let key = format!("br\u{0}{}\u{0}{}", view.title, view.items.len());
                if key == self.cover_job {
                    return;
                }
                let srcs = view
                    .items
                    .iter()
                    .map(|i| match Self::br_item_thumb(i).1 {
                        ThumbSrc::Track(p) => spotify::CoverSrc::Track(p),
                        ThumbSrc::Url(u) => spotify::CoverSrc::Url(u),
                    })
                    .collect();
                (key, srcs)
            }
            _ => return,
        };
        self.cover_job = key;
        let mine = self.cover_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        spotify::prefetch_covers(srcs, self.cover_gen.clone(), mine);
    }

    fn request_visible_br_thumbs(&mut self) {
        if !self.show_album_thumbnails || !self.art.available() {
            return;
        }
        let row_h = self.sp_row_h();
        let visible = (self.rects.br_items.height / row_h) as usize;
        let offset = self.br_item_state.offset();
        let jobs: Vec<(AlbumRef, Vec<ThumbSrc>)> = match self.br_current() {
            Some(v) => thumb_window(offset, visible, v.items.len())
                .map(|i| &v.items[i])
                .map(|i| {
                    let (k, src) = Self::br_item_thumb(i);
                    (k, vec![src])
                })
                .collect(),
            None => return,
        };
        self.spawn_thumb_jobs(jobs);
    }

    /// Start background loads for row thumbnails not cached, loading or
    /// known-missing. Several sources make a 2×2 mosaic. Covers may need a
    /// network fetch, so only a few run at once.
    fn spawn_thumb_jobs(&mut self, jobs: Vec<(AlbumRef, Vec<ThumbSrc>)>) {
        // Pushed in reverse so the first (most urgent) is served first.
        for (key, srcs) in jobs.into_iter().rev() {
            if srcs.is_empty()
                || self.row_thumb_cache.contains_key(&key)
                || self.row_thumb_inflight.contains(&key)
                || self.row_thumb_no_art.contains(&key)
            {
                continue;
            }
            self.row_thumb_inflight.insert(key.clone());
            for gone in self.thumb_pool.push(ThumbJob { key, srcs }) {
                self.row_thumb_inflight.remove(&gone);
            }
        }
    }

    pub fn sp_move(&mut self, delta: i32) {
        let row_h = self.sp_row_h();
        match self.sp_focus {
            PlPane::List => {
                let before = self.sp_list_state.selected();
                move_state(&mut self.sp_list_state, self.sp_view.len(), delta);
                scroll_into_view(&mut self.sp_list_state, (self.rects.sp_list.height / row_h) as usize);
                if self.sp_list_state.selected() != before || delta == 0 {
                    let has = self.selected_sp_list().is_some_and(|p| !p.tracks.is_empty());
                    self.sp_track_state = ListState::default();
                    self.sp_track_state.select(has.then_some(0));
                }
            }
            PlPane::Tracks => {
                let n = self.selected_sp_list().map(|p| p.tracks.len()).unwrap_or(0);
                move_state(&mut self.sp_track_state, n, delta);
                scroll_into_view(&mut self.sp_track_state, (self.rects.sp_tracks.height / row_h) as usize);
            }
            PlPane::Queue => self.queue_move(delta),
        }
    }

    pub fn sp_set_pane(&mut self, pane: PlPane) {
        if !self.panes_focused() {
            self.focus = Focus::Tracks;
        }
        self.sp_focus = pane;
    }

    /// Enter in the Spotify tab: play the selected list (from the chosen track
    /// when the track pane is focused).
    pub fn sp_activate(&mut self) {
        let start = match self.sp_focus {
            PlPane::Tracks => self.sp_track_state.selected().unwrap_or(0),
            PlPane::List => 0,
            PlPane::Queue => return self.queue_play_selected(),
        };
        let Some(tracks) = self.selected_sp_list().map(|p| p.tracks.clone()) else {
            return;
        };
        self.play_tracks(tracks, start);
    }

    /// `a` in the Spotify tab: queue the highlighted track, or the whole list
    /// when the list pane is focused.
    pub fn sp_add_to_queue(&mut self) {
        let Some(list) = self.selected_sp_list() else { return };
        let to_add: Vec<Track> = match self.sp_focus {
            PlPane::List => list.tracks.clone(),
            PlPane::Queue => return,
            PlPane::Tracks => self
                .sp_track_state
                .selected()
                .and_then(|i| list.tracks.get(i).cloned())
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

    /// `x` in the Spotify tab: reject the YouTube match for the highlighted
    /// track (the playing one when the list pane is focused) and fetch the
    /// next-best candidate. A playing track keeps going until the replacement
    /// arrives, then switches over.
    pub fn sp_reject(&mut self) {
        if let Some(t) = self.sp_target_track() {
            self.ask_reject(t);
        }
    }

    /// `x`: ask "wrong song?" before throwing away a fetched match.
    fn ask_reject(&mut self, track: Track) {
        if !spotify::is_remote(&track.path) {
            self.notify("That's one of your own files — nothing to replace.");
            return;
        }
        if spotify::track_is_direct(&track.path) {
            self.notify("That's a specific YouTube video — nothing else to try.");
            return;
        }
        if !track.path.exists() && !self.fetching.contains(&track.path) {
            self.notify("Not downloaded yet — play it first.");
            return;
        }
        self.confirm = Some(Confirm {
            title: " Wrong song? ",
            subject: format!("{} — {}", track.title, track.track_artist),
            detail: "Deletes the download and fetches the next-best YouTube match.".into(),
            yes: "Find another",
            no: "Keep it",
            action: ConfirmAction::Reject(track),
        });
    }

    pub fn confirm_answer(&mut self, yes: bool) {
        let Some(c) = self.confirm.take() else { return };
        if !yes {
            return;
        }
        match c.action {
            ConfirmAction::Reject(t) => self.reject_track(t),
            ConfirmAction::Unlike(t) => self.unlike(&t),
            ConfirmAction::SpDelete => self.sp_delete_now(),
            ConfirmAction::DeletePlaylist => self.delete_playlist_now(),
        }
    }

    fn reject_track(&mut self, track: Track) {
        if !spotify::is_remote(&track.path) {
            return;
        }
        if spotify::track_is_direct(&track.path) {
            self.notify("That's a specific YouTube video — nothing else to try.");
            return;
        }
        if self.fetching.contains(&track.path) {
            self.notify("Still fetching that one…");
            return;
        }
        if !track.path.exists() {
            self.notify("Not fetched yet — nothing to reject.");
            return;
        }
        let playing = self.now.and_then(|i| self.queue.get(i)).is_some_and(|t| t.path == track.path);
        if playing {
            self.pending_play = Some(track.path.clone());
        }
        self.request_fetch(&track, true);
        self.notify(format!("Rejected match for '{}' — finding another…", track.title));
    }

    /// Start a background YouTube fetch for a Spotify track (no-op if one is
    /// already running for it).
    fn request_fetch(&mut self, track: &Track, reject: bool) {
        if !self.fetching.insert(track.path.clone()) {
            return;
        }
        let tx = self.fetch_tx.clone();
        let track = track.clone();
        std::thread::spawn(move || {
            let result = spotify::fetch_audio(&track, reject).map_err(|e| e.to_string());
            let _ = tx.send(FetchMsg { path: track.path, result });
        });
    }

    /// Fetch the upcoming queue entry ahead of time so the crossfade into it
    /// isn't held up. Skipped in shuffle, where the next pick isn't known yet.
    fn prefetch_next(&mut self) {
        if self.shuffle {
            return;
        }
        if let Some(i) = self.peek_next_index()
            && spotify::needs_fetch(&self.queue[i].path)
        {
            let track = self.queue[i].clone();
            self.request_fetch(&track, false);
        }
    }

    /// Drain finished fetches: record real lengths, and start the pending
    /// track if it was the one waiting. Returns true if anything changed.
    fn poll_fetch(&mut self) -> bool {
        let mut changed = false;
        let mut store_dirty = false;
        while let Ok(msg) = self.fetch_rx.try_recv() {
            changed = true;
            self.fetching.remove(&msg.path);
            let waiting = self.pending_play.as_ref() == Some(&msg.path);
            match msg.result {
                Ok(got) => {
                    let lists = self
                        .spotify_lists
                        .iter_mut()
                        .chain(self.sp_view.iter_mut())
                        .chain(self.sp_store.iter_mut())
                        .chain(self.playlists.iter_mut())
                        .flat_map(|p| p.tracks.iter_mut());
                    // Keep each copy's title/artist/album (and list position);
                    // take what the fetch filled in.
                    for t in self.queue.iter_mut().chain(lists) {
                        if t.path == msg.path {
                            t.length_secs = got.length_secs;
                            t.album_artist = got.album_artist.clone();
                            t.track_no = got.track_no;
                            t.disc_no = got.disc_no;
                            t.year = got.year;
                            if t.album.is_empty() {
                                t.album = got.album.clone();
                            }
                        }
                    }
                    store_dirty |= self.sp_store.iter().any(|p| p.tracks.iter().any(|t| t.path == msg.path));
                    if waiting
                        && let Some(i) = self.now
                        && self.queue.get(i).is_some_and(|t| t.path == msg.path)
                    {
                        self.play_index(i);
                    }
                }
                Err(e) => {
                    if waiting {
                        self.pending_play = None;
                        let title = self
                            .queue
                            .iter()
                            .find(|t| t.path == msg.path)
                            .map(|t| t.title.clone())
                            .unwrap_or_default();
                        self.now_playing.clear();
                        self.notify(format!("YouTube fetch failed for '{title}': {e}"));
                    }
                }
            }
        }
        if store_dirty {
            spotify::save_store(&self.sp_store);
        }
        changed
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
                if self.art_cache.len() > 16 {
                    self.art_cache.clear();
                }
                self.art_cache.insert(msg.path.clone(), img.clone());
            }
            if self.art_path.as_deref() == Some(msg.path.as_path()) {
                self.set_current_art(msg.image.clone());
            }
        }
    }

    /// Number of text rows each Albums-column row occupies (thumbnail + title on
    /// row 1, blank spacer on row 2). Shared by the row-position math in
    /// `ui::draw_body` and the viewport calc below, so they never drift apart.
    pub const ALBUM_ROW_HEIGHT: u16 = 2;

    /// Kick off background decodes for Albums-column thumbnails scrolled into
    /// view. Capped per call so a fast PageDown/End jump doesn't burst-spawn a
    /// thread per album at once — the rest catch up over the following ticks.
    /// Row thumbnails kept decoded in memory (each is tiny).
    const ROW_THUMB_CAP: usize = 768;

    /// True while row thumbnails are loading, so the UI polls quickly and
    /// shows each one as soon as it's ready.
    pub fn thumbs_pending(&self) -> bool {
        !self.row_thumb_inflight.is_empty()
    }

    fn request_visible_row_thumbs(&mut self) {
        if !self.show_album_thumbnails || !self.art.available() {
            return;
        }
        let area = self.rects.albums;
        if area.height == 0 {
            return;
        }
        let visible = (area.height / Self::ALBUM_ROW_HEIGHT).max(1) as usize;
        let offset = self.album_state.offset();
        let jobs: Vec<(AlbumRef, Vec<ThumbSrc>)> = thumb_window(offset, visible, self.view_albums.len())
            .map(|i| &self.view_albums[i])
            // The synthetic "All" row has no single cover.
            .filter(|a| !(a.album_artist.is_empty() && a.year.is_none() && a.title == "All"))
            .filter(|a| !self.row_thumb_cache.contains_key(*a) && !self.row_thumb_inflight.contains(*a))
            .filter_map(|a| Some((a.clone(), vec![ThumbSrc::Track(self.library.album_cover_track(a)?.path.clone())])))
            .collect();
        self.spawn_thumb_jobs(jobs);
    }

    /// Drain decoded row thumbnails and build their render protocols. Returns
    /// true if anything changed, so the caller knows to redraw.
    fn poll_row_thumb(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.row_thumb_rx.try_recv() {
            self.row_thumb_inflight.remove(&msg.album);
            match msg.image.and_then(|img| self.art.protocol_from_image(img)) {
                Some(proto) => {
                    // Evict oldest-first, so what's on screen never blinks out.
                    while self.row_thumb_order.len() >= Self::ROW_THUMB_CAP {
                        if let Some(old) = self.row_thumb_order.pop_front() {
                            self.row_thumb_cache.remove(&old);
                        }
                    }
                    if self.row_thumb_cache.insert(msg.album.clone(), proto).is_none() {
                        self.row_thumb_order.push_back(msg.album);
                    }
                    changed = true;
                }
                None => {
                    if self.row_thumb_no_art.len() > 1024 {
                        self.row_thumb_no_art.clear();
                    }
                    self.row_thumb_no_art.insert(msg.album);
                }
            }
        }
        changed
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

    pub fn toggle_album_thumbnails(&mut self) {
        self.show_album_thumbnails = !self.show_album_thumbnails;
        self.config.ui.show_album_thumbnails = self.show_album_thumbnails;
        let _ = self.config.save();
        if !self.show_album_thumbnails {
            self.row_thumb_cache.clear();
            self.row_thumb_inflight.clear();
            self.row_thumb_no_art.clear();
        }
    }

    /// Build cover thumbnails for the whole library on a background thread so
    /// they're cached on disk and load instantly later.
    pub fn start_art_precache(&self) {
        // One track per album: tracks sharing a cover share a thumbnail, so
        // there's no point re-reading every track's tags.
        let mut seen = HashSet::new();
        let albums: Vec<(AlbumRef, PathBuf)> = self
            .library
            .tracks
            .iter()
            .filter(|t| seen.insert((&t.album_artist, &t.album, t.year)))
            .map(|t| {
                let key = AlbumRef { album_artist: t.album_artist.clone(), title: t.album.clone(), year: t.year };
                (key, t.path.clone())
            })
            .collect();
        // Builds both the now-playing size and the row thumbnail on disk, so
        // every album's cover is instant later.
        std::thread::spawn(move || {
            for (key, p) in albums {
                let _ = row_thumb(&key, &[ThumbSrc::Track(p)]);
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
                scroll_into_view(&mut self.playlist_state, self.rects.pl_list.height as usize);
                let sel = self.playlist_state.selected().unwrap_or(0);
                let has = self
                    .playlists
                    .get(sel)
                    .map(|p| !p.tracks.is_empty())
                    .unwrap_or(false);
                self.pl_track_state.select(if has { Some(0) } else { None });
                scroll_into_view(&mut self.pl_track_state, self.rects.pl_tracks.height as usize);
            }
            PlPane::Tracks | PlPane::Queue => {
                let n = self
                    .playlist_state
                    .selected()
                    .and_then(|i| self.playlists.get(i))
                    .map(|p| p.tracks.len())
                    .unwrap_or(0);
                move_state(&mut self.pl_track_state, n, delta);
                scroll_into_view(&mut self.pl_track_state, self.rects.pl_tracks.height as usize);
            }
        }
    }

    pub fn pl_set_pane(&mut self, pane: PlPane) {
        if !self.panes_focused() {
            self.focus = Focus::Tracks;
        }
        self.pl_focus = pane;
    }

    /// Enter in the Playlists tab: play the selected playlist (from the chosen
    /// track when the track pane is focused).
    pub fn pl_activate(&mut self) {
        let start = match self.pl_focus {
            PlPane::Tracks | PlPane::Queue => self.pl_track_state.selected().unwrap_or(0),
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
        scroll_into_view(&mut self.playlist_state, self.rects.pl_list.height as usize);
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
        let Some(p) = self.playlist_state.selected().and_then(|i| self.playlists.get(i)) else { return };
        self.confirm = Some(Confirm {
            title: " Delete playlist? ",
            subject: p.name.clone(),
            detail: format!("{} songs. The music files themselves aren't touched.", p.tracks.len()),
            yes: "Delete",
            no: "Cancel",
            action: ConfirmAction::DeletePlaylist,
        });
    }

    fn delete_playlist_now(&mut self) {
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

    pub fn begin_edit_path(&mut self, field: PathField) {
        let current = match field {
            PathField::MusicDir => &self.config.music_dir,
            PathField::SpotifyCsv => &self.config.spotify.csv,
        };
        self.editing_path = Some(current.to_string_lossy().into_owned());
        self.editing_field = field;
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
                let path = PathBuf::from(shellexpand_home(trimmed));
                match self.editing_field {
                    PathField::MusicDir => {
                        self.set_music_dir(path);
                        self.notify(format!("Music dir set to {}", self.config.music_dir.display()));
                    }
                    PathField::SpotifyCsv => {
                        self.config.spotify.csv = path;
                        let _ = self.config.save();
                        if self.config.spotify.enabled {
                            self.load_spotify();
                            if let Some(e) = &self.spotify_error {
                                self.notify(format!("Spotify export: {e}"));
                            }
                        }
                    }
                }
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
        let fetch_dirty = self.poll_fetch() | self.poll_links() | self.poll_browser();
        self.maybe_keep_going();
        let mut row_thumbs_dirty = false;
        if self.tab == Tab::Local {
            self.request_visible_row_thumbs();
            row_thumbs_dirty = self.poll_row_thumb();
        } else if self.tab == Tab::Streaming {
            self.prefetch_open_list_covers();
            self.request_visible_sp_thumbs();
            row_thumbs_dirty = self.poll_row_thumb();
        } else if self.tab == Tab::Settings {
            self.refresh_cache_stats();
        } else if self.tab == Tab::Browser {
            self.ensure_browser_home();
            self.br_maybe_search();
            self.prefetch_open_list_covers();
            self.request_visible_br_thumbs();
            row_thumbs_dirty = self.poll_row_thumb();
        }
        self.audio.update();

        // Start an automatic crossfade into the next track near the end.
        if self.audio.is_playing() && self.audio.should_start_crossfade()
            && self.pending_play.is_none()
            && let Some(i) = self.peek_next_index() {
                let track = self.queue[i].clone();
                if spotify::needs_fetch(&track.path) {
                    self.request_fetch(&track, false);
                } else if (track.length_secs as f32) > self.audio.crossfade_secs()
                    && self.audio.crossfade_to(&track.path, track.length_secs)
                {
                    self.now = Some(i);
                    self.queue_state.select(Some(i));
                    scroll_into_view(&mut self.queue_state, self.rects.queue.height as usize);
                    self.set_now_playing(&track);
                    self.request_art(track.path.clone());
                    self.prefetch_next();
                }
            }

        if self.last_session_at.elapsed() >= Duration::from_secs(5) {
            self.last_session_at = Instant::now();
            self.save_session();
        }

        // While a fetch is pending, `poll_fetch` starts the next track.
        if self.audio.finished() && self.pending_play.is_none() {
            self.next_track(false);
        }

        // The art popup is static; don't re-render it on every tick (each kitty
        // re-placement of a large image is wasteful). Events still redraw.
        let mut dirty = row_thumbs_dirty
            || fetch_dirty
            || (!self.show_art_popup && (self.audio.is_playing() || self.scanning.is_some()));
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

/// On-disk thumbnail for a cover, keyed by a hash of the embedded image bytes
/// rather than the track path, so every track of an album shares one file.
fn art_thumb_path(cover: &[u8]) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(cover, &mut hasher);
    // The thumbnail size is part of the filename so bumping it invalidates old
    // cached thumbnails instead of reusing them.
    crate::config::art_cache_dir().join(format!(
        "{:016x}-{ART_THUMB_PX}.jpg",
        std::hash::Hasher::finish(&hasher)
    ))
}

/// Load a track's cover. Covers already within `ART_THUMB_PX` are decoded
/// straight from the tag and never touch the disk cache; oversized ones are
/// downscaled once and stored as a JPEG thumbnail shared by every track that
/// embeds the same image.
fn load_cover_cached(path: &std::path::Path) -> Option<DynamicImage> {
    if spotify::is_remote(path) {
        return spotify::load_cover(path);
    }
    let raw = scan::read_cover(path)?;
    let thumb = art_thumb_path(&raw);

    if let Ok(bytes) = std::fs::read(&thumb)
        && let Ok(img) = image::load_from_memory(&bytes) {
            return Some(img);
        }

    let img = image::load_from_memory(&raw).ok()?;
    if img.width() <= ART_THUMB_PX && img.height() <= ART_THUMB_PX {
        return Some(img);
    }
    let small = DynamicImage::ImageRgb8(
        img.resize(ART_THUMB_PX, ART_THUMB_PX, image::imageops::FilterType::Triangle)
            .into_rgb8(),
    );
    let _ = save_thumb(&small, &thumb);
    Some(small)
}

/// Row thumbnails are drawn about 4×2 cells, so this is plenty even on
/// HiDPI. Part of the cache file name, so changing it rebuilds them.
const ROW_THUMB_PX: u32 = 96;

/// A row thumbnail, from the disk cache or built from its covers (then
/// cached). The cache name covers the row and its sources (a local file's
/// size and modification time too), so edited covers and changed playlist
/// mosaics get rebuilt. Missing art isn't cached: it may arrive later.
fn row_thumb(key: &AlbumRef, srcs: &[ThumbSrc]) -> Option<DynamicImage> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    for s in srcs {
        match s {
            ThumbSrc::Track(p) => {
                p.hash(&mut h);
                if !spotify::is_remote(p)
                    && let Ok(m) = std::fs::metadata(p)
                {
                    m.len().hash(&mut h);
                    m.modified().ok().hash(&mut h);
                }
            }
            ThumbSrc::Url(u) => u.hash(&mut h),
        }
    }
    let file = row_thumb_dir().join(format!("{:016x}-{ROW_THUMB_PX}.jpg", h.finish()));
    if let Ok(bytes) = std::fs::read(&file)
        && let Ok(img) = image::load_from_memory(&bytes)
    {
        return Some(img);
    }
    let covers: Vec<DynamicImage> = srcs
        .iter()
        .filter_map(|s| match s {
            ThumbSrc::Track(p) => load_cover_cached(p),
            ThumbSrc::Url(u) if !u.is_empty() => spotify::url_image(u),
            ThumbSrc::Url(_) => None,
        })
        .collect();
    let img = cover_mosaic(covers)?;
    let _ = save_thumb(&img, &file);
    Some(img)
}

fn row_thumb_dir() -> PathBuf {
    crate::config::art_cache_dir().with_file_name("thumbs")
}

/// Row-thumbnail image from a set of covers: a 2×2 grid when there are four,
/// otherwise just the first cover.
fn cover_mosaic(covers: Vec<DynamicImage>) -> Option<DynamicImage> {
    use image::imageops::FilterType;
    const PX: u32 = ROW_THUMB_PX;
    if covers.len() < 4 {
        return covers
            .into_iter()
            .next()
            .map(|c| DynamicImage::ImageRgb8(c.resize(PX, PX, FilterType::Triangle).into_rgb8()));
    }
    let mut grid = image::RgbImage::new(PX, PX);
    for (i, c) in covers.iter().take(4).enumerate() {
        let tile = c.resize_to_fill(PX / 2, PX / 2, FilterType::Triangle).into_rgb8();
        let (x, y) = ((i as u32 % 2) * PX / 2, (i as u32 / 2) * PX / 2);
        image::imageops::overlay(&mut grid, &tile, x as i64, y as i64);
    }
    Some(DynamicImage::ImageRgb8(grid))
}

/// Write a JPEG thumbnail via a temp file + rename, so concurrent loaders of
/// the same cover never read a half-written file.
fn save_thumb(img: &DynamicImage, dest: &std::path::Path) -> anyhow::Result<()> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, 85).encode_image(img)?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Remove cover caches left by older versions: the per-track PNG `art/` dir,
/// and any thumbnails in the current dir that don't match today's name scheme.
fn prune_legacy_art_cache() {
    std::thread::spawn(|| {
        if let Some(legacy) = crate::config::legacy_art_cache_dir() {
            let _ = std::fs::remove_dir_all(legacy);
        }
        let dir = crate::config::art_cache_dir();
        for (dir, px) in [(dir, ART_THUMB_PX), (row_thumb_dir(), ROW_THUMB_PX)] {
            let suffix = format!("-{px}.jpg");
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                if !e.file_name().to_string_lossy().ends_with(&suffix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    });
}

/// Row indices to load thumbnails for, most urgent first: the rows in view,
/// then two screens below, then a screen above, so covers are ready before
/// they're scrolled to.
/// Count and total size of the files under `dir`.
fn dir_stats(dir: &std::path::Path) -> DirStats {
    let mut s = DirStats::default();
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if let Ok(m) = e.metadata()
            && m.is_file()
        {
            s.files += 1;
            s.bytes += m.len();
        }
    }
    s
}

/// Open a directory in the desktop's file manager (best-effort, detached).
fn open_in_file_manager(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::process::Command::new("xdg-open")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn thumb_window(offset: usize, visible: usize, len: usize) -> impl Iterator<Item = usize> {
    let end = (offset + visible).min(len);
    let below = end..(end + 2 * visible).min(len);
    let above = offset.saturating_sub(visible)..offset;
    (offset.min(len)..end).chain(below).chain(above.rev())
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

/// Nudge `state`'s offset so its selection stays within a `visible`-row
/// window. `render_list` (ui.rs) no longer lets `List` auto-scroll to follow
/// the selection on every render — that auto-follow is exactly what fought a
/// scrollbar-drag trying to pan the view independent of the current
/// selection — so anything that moves `selected` via a non-click path (arrow
/// keys, jumping to the end of a list, auto-advance) needs to restore that
/// "keep it visible" behavior explicitly.
fn scroll_into_view(state: &mut ListState, visible: usize) {
    if visible == 0 {
        return;
    }
    let Some(sel) = state.selected() else { return };
    let offset = state.offset();
    if sel < offset {
        *state.offset_mut() = sel;
    } else if sel >= offset + visible {
        *state.offset_mut() = sel + 1 - visible;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_round_trips_through_toml() {
        let t = Track {
            id: 3,
            path: PathBuf::from("/m/a.flac"),
            title: "A".into(),
            track_artist: "B".into(),
            album_artist: "B".into(),
            album: "C".into(),
            year: Some(1999),
            track_no: Some(1),
            disc_no: None,
            length_secs: 200,
        };
        let s = Session {
            tab: "Streaming".into(),
            queue: vec![t.clone(), t],
            now: Some(1),
            position_secs: 83.5,
            volume: Some(0.6),
            repeat: "All".into(),
            artist: Some(4),
            ..Default::default()
        };
        let body = toml::to_string(&s).unwrap();
        let back: Session = toml::from_str(&body).unwrap();
        assert_eq!(back.queue.len(), 2);
        assert_eq!((back.now, back.position_secs, back.artist), (Some(1), 83.5, Some(4)));
        assert_eq!(back.repeat, "All");
    }
}
