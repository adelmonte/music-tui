//! What each named action does in each tab, "play next", and the
//! right-click menu built from those actions.

use super::*;
use crate::keys::Action;
use crate::menu::{Menu, MenuCmd, MenuItem};

impl App {
    /// Run `action` where it applies. False if it means nothing here, so
    /// the next action bound to the same key gets a turn.
    pub fn run_action(&mut self, action: Action) -> bool {
        use Action as A;
        let tab = self.tab;
        // Outside Local, the bars at the bottom (progress, playback buttons,
        // volume) take the arrows and Enter when focused.
        if tab != Tab::Local && tab != Tab::Settings && !self.panes_focused() && self.focus != Focus::Search {
            match action {
                A::Left => return { self.on_left(); true },
                A::Right => return { self.on_right(); true },
                A::Up if self.focus == Focus::Volume => return { self.volume_up(); true },
                A::Down if self.focus == Focus::Volume => return { self.volume_down(); true },
                A::Up | A::Down => return true,
                A::Activate => return { self.activate(); true },
                A::Back => return { self.set_focus(Focus::Tracks); true },
                _ => {}
            }
        }
        let queue = self.queue_focused();
        match action {
            A::Quit => self.should_quit = true,
            A::ShowKeys => self.show_keys = !self.show_keys,
            A::Help => self.show_help = true,
            A::Art => self.open_art_popup(),
            A::Tab1 | A::Tab2 | A::Tab3 | A::Tab4 | A::Tab5 => {
                let n = [A::Tab1, A::Tab2, A::Tab3, A::Tab4, A::Tab5].iter().position(|a| *a == action).unwrap_or(0);
                return self.goto_tab(n + 1);
            }
            A::Menu => {
                let pos = self.menu_anchor();
                self.open_context_menu(pos);
            }
            A::FocusNext => self.cycle_focus(true),
            A::FocusPrev => self.cycle_focus(false),
            A::Search if matches!(tab, Tab::Local | Tab::Streaming | Tab::Browser) => self.set_focus(Focus::Search),
            A::Back => match tab {
                Tab::Browser => {
                    if !self.br_back() {
                        self.clear_search();
                    }
                }
                Tab::Settings | Tab::Playlists => return false,
                _ => self.clear_search(),
            },

            A::Up | A::Down | A::PageUp | A::PageDown | A::First | A::Last => {
                let delta = match action {
                    A::Up => -1,
                    A::Down => 1,
                    A::PageUp => -10,
                    A::PageDown => 10,
                    A::First => -100_000,
                    _ => 100_000,
                };
                match tab {
                    Tab::Local if action == A::Up => self.on_up(),
                    Tab::Local if action == A::Down => self.on_down(),
                    Tab::Local => self.move_selection(delta),
                    Tab::Streaming => self.sp_move(delta),
                    Tab::Browser => self.br_move(delta),
                    Tab::Playlists => self.pl_move(delta),
                    Tab::Settings => self.settings_move(delta),
                }
            }
            A::Left | A::Right => {
                let right = action == A::Right;
                let pane = if right { PlPane::Tracks } else { PlPane::List };
                match tab {
                    Tab::Local if right => self.on_right(),
                    Tab::Local => self.on_left(),
                    Tab::Streaming => self.sp_set_pane(pane),
                    Tab::Browser => self.br_set_pane(pane),
                    Tab::Playlists => self.pl_set_pane(pane),
                    Tab::Settings => self.settings_adjust(if right { 1 } else { -1 }),
                }
            }
            A::Activate => match tab {
                Tab::Local => self.activate(),
                Tab::Streaming | Tab::Browser if queue => self.queue_play_selected(),
                Tab::Streaming => self.sp_activate(),
                Tab::Browser => self.br_activate(),
                Tab::Playlists => self.pl_activate(),
                Tab::Settings => self.settings_activate(),
            },
            A::Wider | A::Narrower => {
                let wider = action == A::Wider;
                match tab {
                    Tab::Local => self.resize_focused(wider),
                    Tab::Streaming | Tab::Browser => self.sp_resize(wider),
                    _ => return false,
                }
            }

            A::PlayPause => self.toggle_pause(),
            A::Next => self.next_track(true),
            A::Prev => self.prev_track(),
            A::Shuffle => self.toggle_shuffle(),
            A::Repeat => self.cycle_repeat(),
            A::VolumeUp => self.volume_up(),
            A::VolumeDown => self.volume_down(),
            A::SeekBack => self.seek_relative(-5),
            A::SeekForward => self.seek_relative(5),
            A::KeepGoing => self.toggle_keep_going(),

            A::NewPlaylist if tab == Tab::Playlists => self.begin_new_playlist(),
            A::AppendQueue if tab == Tab::Playlists => self.append_queue_to_playlist(),
            A::RemoveSong if tab == Tab::Playlists => self.remove_pl_track(),
            A::Queue => match tab {
                Tab::Local if !queue => self.add_to_queue(),
                Tab::Streaming if !queue => self.sp_add_to_queue(),
                Tab::Browser if !queue => self.br_add_to_queue(),
                _ => return false,
            },
            A::PlayNext if matches!(tab, Tab::Local | Tab::Streaming | Tab::Browser) => self.play_next(),
            A::Like if matches!(tab, Tab::Local | Tab::Streaming | Tab::Browser) => self.like_current(),
            A::AddTo if matches!(tab, Tab::Local | Tab::Streaming | Tab::Browser) => self.open_add_menu(),
            A::GoTo if tab != Tab::Settings => self.open_goto_menu(),
            A::Radio if tab != Tab::Settings => match self.radio_target() {
                Some(t) => self.start_radio(t),
                None => self.notify("Highlight a song first."),
            },
            A::Reject if tab == Tab::Streaming && !queue => self.sp_reject(),
            A::Reject if tab == Tab::Browser && !queue => self.br_reject(),
            A::Remove => match tab {
                _ if queue => self.remove_from_queue(),
                Tab::Streaming => self.sp_delete(),
                Tab::Playlists => self.delete_playlist(),
                _ => return false,
            },
            A::ClearQueue if tab == Tab::Local || queue => self.clear_queue(),
            A::MoveUp if queue => self.move_queue_item(-1),
            A::MoveDown if queue => self.move_queue_item(1),
            A::ToggleQueue if tab != Tab::Settings => self.toggle_queue(),
            A::Sort if tab == Tab::Streaming => self.sp_cycle_sort(),
            A::SaveList if tab == Tab::Browser => self.br_save_view(),
            A::LinkPlaylist if tab == Tab::Streaming => self.begin_link_input(),
            A::Reimport if tab == Tab::Streaming => self.reimport_spotify(),
            _ => return false,
        }
        true
    }

    /// Put the highlighted song(s) right after the one playing. In the
    /// queue, move the highlighted entry there instead.
    pub fn play_next(&mut self) {
        if self.queue_focused() {
            let Some(i) = self.queue_state.selected().filter(|&i| i < self.queue.len()) else { return };
            if Some(i) == self.now {
                return;
            }
            let t = self.queue.remove(i);
            let title = t.title.clone();
            self.now = self.now.map(|n| if i < n { n - 1 } else { n });
            let at = self.now.map_or(0, |n| n + 1);
            self.queue.insert(at, t);
            self.queue_state.select(Some(at));
            self.prefetch_next();
            self.notify(format!("'{title}' plays next."));
            return;
        }
        let before = self.queue.len();
        match self.tab {
            Tab::Local => self.add_to_queue(),
            Tab::Streaming => self.sp_add_to_queue(),
            Tab::Browser => self.br_add_to_queue(),
            _ => return,
        }
        let added = self.queue.len() - before;
        if added == 0 {
            return;
        }
        let at = self.now.map_or(0, |n| n + 1).min(before);
        let tail: Vec<Track> = self.queue.drain(before..).collect();
        let first = tail[0].title.clone();
        self.queue.splice(at..at, tail);
        if let Some(sel) = self.queue_state.selected()
            && sel >= at
            && sel < before
        {
            self.queue_state.select(Some(sel + added));
        }
        self.prefetch_next();
        self.notify(if added == 1 {
            format!("'{first}' plays next.")
        } else {
            format!("{added} songs play next.")
        });
    }

    /// Run something picked from the right-click menu.
    pub fn run_menu_cmd(&mut self, cmd: MenuCmd) {
        match cmd {
            MenuCmd::Act(a) => {
                self.run_action(a);
            }
            MenuCmd::Play => match self.tab {
                Tab::Local => self.activate_play(),
                _ => {
                    self.run_action(Action::Activate);
                }
            },
            MenuCmd::AddTo(target) => {
                self.open_add_menu();
                let Some(i) = self.add_targets().iter().position(|(_, t)| *t == target) else { return };
                if let Some(m) = &mut self.add_menu {
                    m.sel = i;
                }
                self.add_menu_choose();
            }
            MenuCmd::Goto(kind) => {
                let Some(track) = self.radio_target() else { return };
                self.goto_menu = Some(GotoMenu { track, sel: 0 });
                let key = match kind {
                    GotoKind::Radio => 'r',
                    GotoKind::Artist => 'a',
                    GotoKind::Album => 'l',
                    GotoKind::KeepGoing => 'c',
                };
                self.goto_choose(Some(key));
            }
            MenuCmd::Sort(s) => self.sp_set_sort(s),
        }
    }

    /// Open the menu for whatever is highlighted, at `pos`.
    pub fn open_context_menu(&mut self, pos: (u16, u16)) {
        let (title, items) = self.menu_items();
        if items.is_empty() {
            return;
        }
        self.show_keys = false;
        self.menu = Some(Menu::new(title, items, pos));
    }

    /// Where the keyboard-opened menu goes: beside the highlighted row.
    fn menu_anchor(&self) -> (u16, u16) {
        let r = &self.rects;
        let row_h = self.sp_row_h();
        let album_h = if self.show_album_thumbnails { Self::ALBUM_ROW_HEIGHT } else { 1 };
        let (rect, state, h) = match self.tab {
            _ if self.queue_focused() => (r.queue, &self.queue_state, 1),
            Tab::Local => match self.focus {
                Focus::Artists => (r.artists, &self.artist_state, 1),
                Focus::Albums => (r.albums, &self.album_state, album_h),
                _ => (r.tracks, &self.track_state, 1),
            },
            Tab::Streaming if self.sp_focus == PlPane::List => (r.sp_list, &self.sp_list_state, row_h),
            Tab::Streaming => (r.sp_tracks, &self.sp_track_state, row_h),
            Tab::Browser if self.br_focus == PlPane::List => (r.br_sections, &self.br_sec_state, 1),
            Tab::Browser => (r.br_items, &self.br_item_state, row_h),
            Tab::Playlists if self.pl_focus == PlPane::List => (r.pl_list, &self.playlist_state, 1),
            _ => (r.pl_tracks, &self.pl_track_state, 1),
        };
        let row = state.selected().unwrap_or(0).saturating_sub(state.offset()) as u16;
        let y = (rect.y + row * h + 1).min(rect.bottom().saturating_sub(1));
        (rect.x + (rect.width / 3).min(24), y)
    }

    /// The menu for the highlighted item: a title and its commands.
    fn menu_items(&self) -> (Option<String>, Vec<MenuItem>) {
        let k = |a: Action| self.keymap.label(a);
        let act = |label: &str, a: Action| MenuItem::new(label, k(a), MenuCmd::Act(a));
        let goto = |label: &str, g: GotoKind, key: String| MenuItem::new(label, key, MenuCmd::Goto(g));
        let song_title = |t: &Track| format!("{} — {}", t.title, t.track_artist);
        let streaming = self.config.spotify.enabled;

        // Things to do with one song, shared by every tab.
        let song_items = |t: &Track, play: bool| {
            let mut v = Vec::new();
            if play {
                v.push(MenuItem::new("Play", k(Action::Activate), MenuCmd::Play));
            }
            v.push(act("Play next", Action::PlayNext));
            v.push(act("Add to queue", Action::Queue));
            v.push(MenuItem::sep());
            if streaming {
                let liked = self.liked.contains(&t.path);
                v.push(act(if liked { "Unlike" } else { "Like" }, Action::Like));
            }
            v.push(self.add_to_submenu());
            v.push(MenuItem::sep());
            v.push(goto("Song radio", GotoKind::Radio, k(Action::Radio)));
            v.push(goto("Go to artist", GotoKind::Artist, String::new()));
            if !t.album.is_empty() {
                v.push(goto("Go to album", GotoKind::Album, String::new()));
            }
            v
        };

        if self.queue_focused() {
            let Some(t) = self.queue_selected_track() else { return (None, Vec::new()) };
            let mut v = vec![
                MenuItem::new("Play", k(Action::Activate), MenuCmd::Play),
                act("Play next", Action::PlayNext),
                act("Move up", Action::MoveUp),
                act("Move down", Action::MoveDown),
                act("Remove from queue", Action::Remove),
                MenuItem::sep(),
            ];
            if streaming && self.tab != Tab::Local {
                let liked = self.liked.contains(&t.path);
                v.push(act(if liked { "Unlike" } else { "Like" }, Action::Like));
                v.push(self.add_to_submenu());
            }
            v.push(goto("Song radio", GotoKind::Radio, k(Action::Radio)));
            v.push(goto("Go to artist", GotoKind::Artist, String::new()));
            v.push(MenuItem::sep());
            v.push(act("Clear queue", Action::ClearQueue));
            v.push(act("Hide queue", Action::ToggleQueue));
            return (Some(song_title(&t)), v);
        }

        match self.tab {
            Tab::Local => match self.focus {
                Focus::Artists | Focus::Albums => {
                    let name = if self.focus == Focus::Artists {
                        self.selected_artist()
                    } else {
                        self.selected_album().map(|a| a.title.clone())
                    };
                    let mut v = vec![
                        MenuItem::new("Play", String::new(), MenuCmd::Play),
                        act("Play next", Action::PlayNext),
                        act("Add to queue", Action::Queue),
                    ];
                    v.push(self.add_to_submenu());
                    (name, v)
                }
                _ => match self.local_target_track() {
                    Some(t) => (Some(song_title(&t)), song_items(&t, true)),
                    None => (None, Vec::new()),
                },
            },
            Tab::Streaming => {
                let Some(list) = self.selected_sp_list() else { return (None, Vec::new()) };
                let linked = list.name.starts_with("↻ ");
                if self.sp_focus == PlPane::List {
                    let mut v = vec![
                        MenuItem::new("Play", k(Action::Activate), MenuCmd::Play),
                        act("Play next", Action::PlayNext),
                        act("Add to queue", Action::Queue),
                        self.add_to_submenu(),
                        MenuItem::sep(),
                        self.sort_submenu(),
                        act("Link a playlist…", Action::LinkPlaylist),
                    ];
                    if linked {
                        v.push(MenuItem::sep());
                        v.push(act("Unlink playlist", Action::Remove));
                    }
                    return (Some(list.name.clone()), v);
                }
                let Some(t) = self.sp_target_track() else { return (None, Vec::new()) };
                let mut v = song_items(&t, true);
                v.push(MenuItem::sep());
                v.push(self.sort_submenu());
                v.push(act("Wrong song? Find another", Action::Reject));
                if !linked {
                    v.push(act("Remove from this list", Action::Remove));
                }
                (Some(song_title(&t)), v)
            }
            Tab::Browser => {
                if self.br_focus == PlPane::List {
                    let Some(view) = self.br_current() else { return (None, Vec::new()) };
                    let v = vec![
                        act("Play next", Action::PlayNext),
                        act("Add all to queue", Action::Queue),
                        self.add_to_submenu(),
                        act("Save as playlist", Action::SaveList),
                    ];
                    return (Some(view.title.clone()), v);
                }
                let Some(view) = self.br_current() else { return (None, Vec::new()) };
                let item = self.br_item_state.selected().and_then(|i| view.items.get(i));
                let mut v = match item {
                    Some(deezer::Item::Track(t)) => {
                        let mut v = song_items(t, true);
                        v.push(MenuItem::sep());
                        v.push(act("Wrong song? Find another", Action::Reject));
                        v
                    }
                    Some(_) => vec![MenuItem::new("Open", k(Action::Activate), MenuCmd::Play)],
                    None => Vec::new(),
                };
                v.push(MenuItem::sep());
                v.push(act("Save list as playlist", Action::SaveList));
                if !self.br_stack.is_empty() {
                    v.push(act("Back", Action::Back));
                }
                let title = match item {
                    Some(deezer::Item::Track(t)) => song_title(t),
                    _ => view.title.clone(),
                };
                (Some(title), v)
            }
            Tab::Playlists => {
                let Some(p) = self.selected_playlist() else { return (None, Vec::new()) };
                if self.pl_focus == PlPane::List {
                    let v = vec![
                        MenuItem::new("Play", k(Action::Activate), MenuCmd::Play),
                        act("Add the queue to it", Action::AppendQueue),
                        act("New playlist from the queue", Action::NewPlaylist),
                        MenuItem::sep(),
                        act("Delete playlist", Action::Remove),
                    ];
                    return (Some(p.name.clone()), v);
                }
                let t = self.pl_track_state.selected().and_then(|i| p.tracks.get(i));
                let v = vec![
                    MenuItem::new("Play from here", k(Action::Activate), MenuCmd::Play),
                    act("Remove from playlist", Action::RemoveSong),
                    MenuItem::sep(),
                    goto("Song radio", GotoKind::Radio, k(Action::Radio)),
                ];
                (t.map(song_title), v)
            }
            Tab::Settings => (None, Vec::new()),
        }
    }

    fn add_to_submenu(&self) -> MenuItem {
        let items = self
            .add_targets()
            .into_iter()
            .map(|(label, target)| MenuItem::new(label, String::new(), MenuCmd::AddTo(target)))
            .collect();
        let mut m = MenuItem::sub("Add to playlist", items);
        m.key = self.keymap.label(Action::AddTo);
        m
    }

    fn sort_submenu(&self) -> MenuItem {
        let items = [SpSort::Recent, SpSort::Oldest, SpSort::Title, SpSort::Artist, SpSort::Album]
            .into_iter()
            .map(|s| {
                let mark = if s == self.sp_sort { "● " } else { "  " };
                let label = s.label();
                let label = format!("{mark}{}{}", label[..1].to_uppercase(), &label[1..]);
                MenuItem::new(label, String::new(), MenuCmd::Sort(s))
            })
            .collect();
        let mut m = MenuItem::sub("Sort songs", items);
        m.key = self.keymap.label(Action::Sort);
        m
    }

    /// The vim-keys setting.
    pub fn toggle_vim_keys(&mut self) {
        self.config.ui.vim_keys = !self.config.ui.vim_keys;
        let _ = self.config.save();
        self.keymap = crate::keys::Keymap::new(&self.config.keys, self.config.ui.vim_keys).0;
        self.notify(if self.config.ui.vim_keys { "Vim keys on (h/j/k/l, J/K)." } else { "Vim keys off." });
    }

    /// The always-visible hint row setting.
    pub fn toggle_show_hints(&mut self) {
        self.config.ui.show_hints = !self.config.ui.show_hints;
        let _ = self.config.save();
    }
}
