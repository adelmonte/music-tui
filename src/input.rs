use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::app::{App, Drag, Focus, Tab};

/// Translate a key press into app state changes. Modal: help overlay, search
/// field, and library-path editing each capture input first.
pub fn handle_key(app: &mut App, key: KeyEvent) {
    // Ctrl-C always quits.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    if app.show_help {
        // Any key dismisses the help overlay.
        app.show_help = false;
        return;
    }

    if app.show_art_popup {
        // Any key closes the album-art overlay.
        app.show_art_popup = false;
        return;
    }

    if app.editing_path.is_some() {
        match key.code {
            KeyCode::Enter => app.commit_edit_path(),
            KeyCode::Esc => app.cancel_edit_path(),
            KeyCode::Backspace => app.edit_path_backspace(),
            KeyCode::Char(c) => app.edit_path_input(c),
            _ => {}
        }
        return;
    }

    if app.editing_playlist_name.is_some() {
        match key.code {
            KeyCode::Enter => app.commit_new_playlist(),
            KeyCode::Esc => app.cancel_new_playlist(),
            KeyCode::Backspace => app.pl_name_backspace(),
            KeyCode::Char(c) => app.pl_name_input(c),
            _ => {}
        }
        return;
    }

    // Search field is just another focusable element; Tab/arrows move out of it.
    // Within it, readline/Emacs editing keys are supported (Ctrl-A/E/B/F, word
    // motions, kills, yank) — checked before the plain-char insert so a Ctrl/Alt
    // chord is not typed as text.
    if app.is_searching() {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter | KeyCode::Down => app.set_focus(Focus::Artists),
            KeyCode::Tab => app.cycle_focus(true),
            KeyCode::BackTab => app.cycle_focus(false),
            KeyCode::Esc => {
                app.clear_search();
                app.set_focus(Focus::Artists);
            }

            // Motion
            KeyCode::Char('a') if ctrl => app.search_home(),
            KeyCode::Char('e') if ctrl => app.search_end(),
            KeyCode::Char('b') if ctrl => app.search_move(-1),
            KeyCode::Char('f') if ctrl => app.search_move(1),
            KeyCode::Char('b') if alt => app.search_move_word(-1),
            KeyCode::Char('f') if alt => app.search_move_word(1),
            KeyCode::Left if ctrl || alt => app.search_move_word(-1),
            KeyCode::Right if ctrl || alt => app.search_move_word(1),
            KeyCode::Left => app.search_move(-1),
            KeyCode::Right => app.search_move(1),
            KeyCode::Home => app.search_home(),
            KeyCode::End => app.search_end(),

            // Deletion / kill / yank
            KeyCode::Char('d') if ctrl => app.search_delete_forward(),
            KeyCode::Char('d') if alt => app.search_kill_word_forward(),
            KeyCode::Char('w') if ctrl => app.search_kill_word_back(),
            KeyCode::Char('u') if ctrl => app.search_kill_to_start(),
            KeyCode::Char('k') if ctrl => app.search_kill_to_end(),
            KeyCode::Char('y') if ctrl => app.search_yank(),
            KeyCode::Char('h') if ctrl => app.search_backspace(),
            KeyCode::Backspace if alt => app.search_kill_word_back(),
            KeyCode::Delete => app.search_delete_forward(),
            KeyCode::Backspace => app.search_backspace(),

            KeyCode::Char(c) if !ctrl && !alt => app.search_input(c),
            _ => {}
        }
        return;
    }

    // Album-art popup: available from any tab, but not while typing (handled above).
    if key.code == KeyCode::Char('i') {
        app.open_art_popup();
        return;
    }

    // Settings tab has its own small keymap.
    if app.tab == Tab::Settings {
        match key.code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('?') => app.show_help = true,
            KeyCode::Char('1') => app.tab = Tab::Browser,
            KeyCode::Char('2') => app.tab = Tab::Playlists,
            KeyCode::Char('3') => app.tab = Tab::Settings,
            KeyCode::Char('e') => app.begin_edit_path(),
            KeyCode::Char('r') => app.start_scan(),
            KeyCode::Char('c') => app.toggle_cache_all_art(),
            KeyCode::Char('t') => app.toggle_sort_articles(),
            KeyCode::Char('k') => app.toggle_big_icons(),
            KeyCode::Char('v') => app.toggle_album_thumbnails(),
            KeyCode::Char('+') | KeyCode::Char('=') => app.adjust_gapless(0.5),
            KeyCode::Char('-') | KeyCode::Char('_') => app.adjust_gapless(-0.5),
            _ => {}
        }
        return;
    }

    // Playlists tab keymap.
    if app.tab == Tab::Playlists {
        match key.code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('?') => app.show_help = true,
            KeyCode::Char('1') => app.tab = Tab::Browser,
            KeyCode::Char('2') => app.tab = Tab::Playlists,
            KeyCode::Char('3') => app.tab = Tab::Settings,
            KeyCode::Up | KeyCode::Char('k') => app.pl_move(-1),
            KeyCode::Down | KeyCode::Char('j') => app.pl_move(1),
            KeyCode::Left | KeyCode::Char('h') => app.pl_set_pane(crate::app::PlPane::List),
            KeyCode::Right | KeyCode::Char('l') => app.pl_set_pane(crate::app::PlPane::Tracks),
            KeyCode::Enter => app.pl_activate(),
            KeyCode::Char('n') => app.begin_new_playlist(),
            KeyCode::Char('a') => app.append_queue_to_playlist(),
            KeyCode::Char('d') => app.delete_playlist(),
            KeyCode::Char('x') => app.remove_pl_track(),
            KeyCode::Char(' ') => app.toggle_pause(),
            _ => {}
        }
        return;
    }

    // Main view keymap (hybrid arrows + vim).
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('1') => app.tab = Tab::Browser,
        KeyCode::Char('2') => app.tab = Tab::Playlists,
        KeyCode::Char('3') => app.tab = Tab::Settings,
        KeyCode::Char('/') => app.set_focus(Focus::Search),
        KeyCode::Esc => app.clear_search(),
        KeyCode::Tab => app.cycle_focus(true),
        KeyCode::BackTab => app.cycle_focus(false),

        KeyCode::Up | KeyCode::Char('k') => app.on_up(),
        KeyCode::Down | KeyCode::Char('j') => app.on_down(),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::Left | KeyCode::Char('h') => app.on_left(),
        KeyCode::Right | KeyCode::Char('l') => app.on_right(),
        KeyCode::Enter => app.activate(),
        KeyCode::Char('a') => {
            if app.focus == Focus::Queue {
                app.remove_from_queue();
            } else {
                app.add_to_queue();
            }
        }
        KeyCode::Char('c') => app.clear_queue(),

        KeyCode::Char(' ') => app.toggle_pause(),
        KeyCode::Char('n') => app.next_track(true),
        KeyCode::Char('p') => app.prev_track(),
        KeyCode::Char('s') => app.toggle_shuffle(),
        KeyCode::Char('r') => app.cycle_repeat(),
        KeyCode::Char('+') | KeyCode::Char('=') => app.volume_up(),
        KeyCode::Char('-') | KeyCode::Char('_') => app.volume_down(),
        // Alt+] / Alt+[ resize the focused column; plain [ ] seek.
        KeyCode::Char(']') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.resize_focused(true)
        }
        KeyCode::Char('[') if key.modifiers.contains(KeyModifiers::ALT) => {
            app.resize_focused(false)
        }
        KeyCode::Char('[') => app.seek_relative(-5),
        KeyCode::Char(']') => app.seek_relative(5),
        _ => {}
    }
}

fn hit(rect: Rect, x: u16, y: u16) -> bool {
    rect.contains(Position { x, y })
}

/// Map a row click inside a list's inner rect to an item index, honoring scroll
/// and each row's height in text lines (most lists are 1; the Albums column is
/// 2 when thumbnails are on — see `App::ALBUM_ROW_HEIGHT`).
fn row_index(rect: Rect, state: &ListState, y: u16, len: usize, row_h: u16) -> Option<usize> {
    if len == 0 || y < rect.y || row_h == 0 {
        return None;
    }
    let idx = state.offset() + ((y - rect.y) / row_h) as usize;
    if idx < len { Some(idx) } else { None }
}

/// Translate a mouse event into app state changes using the per-frame rects.
pub fn handle_mouse(app: &mut App, ev: MouseEvent) {
    let (x, y) = (ev.column, ev.row);
    match ev.kind {
        MouseEventKind::ScrollDown => {
            if hit(app.rects.volume, x, y) {
                app.volume_down();
            } else {
                if let Some(p) = focus_at(app, x, y) {
                    app.set_focus(p);
                }
                app.move_selection(1);
            }
        }
        MouseEventKind::ScrollUp => {
            if hit(app.rects.volume, x, y) {
                app.volume_up();
            } else {
                if let Some(p) = focus_at(app, x, y) {
                    app.set_focus(p);
                }
                app.move_selection(-1);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => handle_click(app, x, y),
        MouseEventKind::Drag(MouseButton::Left) => handle_drag(app, x, y),
        MouseEventKind::Up(MouseButton::Left) => {
            // Apply the deferred seek now that the drag is done.
            if let Some(Drag::Seek) = app.dragging
                && let Some(frac) = app.pending_seek.take() {
                    app.seek_fraction(frac);
                }
            // Persist layout once, when a resize drag ends.
            match app.dragging {
                Some(Drag::ColDivider(_)) => app.persist_columns(),
                Some(Drag::QueueDivider) => app.persist_queue_pct(),
                _ => {}
            }
            app.dragging = None;
        }
        _ => {}
    }
}

/// While the left button is held, continue scrubbing whatever was grabbed on
/// mouse-down, even if the pointer strays off the bar.
fn handle_drag(app: &mut App, x: u16, y: u16) {
    match app.dragging {
        Some(Drag::Seek) => app.seek_dragging(seek_frac(app.rects.seekbar, x)),
        Some(Drag::Volume) => app.set_volume(vol_frac(app.rects.volume, y)),
        Some(Drag::ColDivider(i)) => app.drag_divider(i, x),
        Some(Drag::QueueDivider) => app.drag_queue_divider(y),
        None => {}
    }
}

/// True if the pointer is on the tracks/queue divider within the third column.
fn near_queue_divider(r: &crate::app::Rects, x: u16, y: u16) -> bool {
    let c = r.col3;
    c.width > 0
        && x >= c.x
        && x < c.x + c.width
        && (y == r.queue_div || (r.queue_div > 0 && y == r.queue_div - 1))
}

/// True if `x` is on (or one cell left of) a column divider line within the body.
fn near_divider(r: &crate::app::Rects, x: u16, y: u16) -> Option<usize> {
    if r.body.height == 0 || y < r.body.y || y >= r.body.y + r.body.height {
        return None;
    }
    (0..2).find(|&i| {
        let dx = r.col_div[i];
        x == dx || (dx > 0 && x == dx - 1)
    })
}

/// Horizontal position within the seek bar clamped to 0.0..=1.0.
fn seek_frac(r: Rect, x: u16) -> f64 {
    if r.width == 0 {
        return 0.0;
    }
    let rel = (x as i32 - r.x as i32).clamp(0, r.width as i32);
    rel as f64 / r.width as f64
}

/// Vertical position within the volume slider (top = 1.0, bottom = 0.0).
fn vol_frac(r: Rect, y: u16) -> f64 {
    let span = (r.height as f64 - 1.0).max(1.0);
    let rel = (y as i32 - r.y as i32).clamp(0, span as i32);
    1.0 - rel as f64 / span
}

/// Set focus to whichever column the pointer is over (without selecting a row).
fn focus_at(app: &App, x: u16, y: u16) -> Option<Focus> {
    let r = &app.rects;
    let p = if hit(r.artists, x, y) {
        Focus::Artists
    } else if hit(r.albums, x, y) {
        Focus::Albums
    } else if hit(r.tracks, x, y) {
        Focus::Tracks
    } else if hit(r.queue, x, y) {
        Focus::Queue
    } else {
        return None;
    };
    Some(p)
}

fn handle_click(app: &mut App, x: u16, y: u16) {
    if app.show_help {
        app.show_help = false;
        return;
    }
    if app.show_art_popup {
        app.show_art_popup = false;
        return;
    }
    // A fresh click ends any prior drag grab.
    app.dragging = None;
    let r = app.rects.clone();
    let double = app.is_double_click(x, y);

    // Tab bar.
    if hit(r.tab_browser, x, y) {
        app.tab = Tab::Browser;
        return;
    }
    if hit(r.tab_playlists, x, y) {
        app.tab = Tab::Playlists;
        return;
    }
    if hit(r.tab_settings, x, y) {
        app.tab = Tab::Settings;
        return;
    }
    // "? help" hint in the top-right.
    if hit(r.help_hint, x, y) {
        app.show_help = true;
        return;
    }

    // Transport buttons: focus the row, select the button, and trigger it.
    for (i, rect) in [
        r.btn_shuffle,
        r.btn_prev,
        r.btn_play,
        r.btn_next,
        r.btn_repeat,
    ]
    .into_iter()
    .enumerate()
    {
        if hit(rect, x, y) {
            app.set_focus(Focus::Transport);
            app.transport_sel = i;
            app.activate();
            return;
        }
    }

    // Volume slider — grab for drag, then set from the click position.
    if hit(r.volume, x, y) && r.volume.height > 0 {
        app.set_focus(Focus::Volume);
        app.dragging = Some(Drag::Volume);
        app.set_volume(vol_frac(r.volume, y));
        return;
    }

    // Seek bar — grab for drag; first click fires an immediate seek.
    if hit(r.seekbar, x, y) && r.seekbar.width > 0 {
        app.set_focus(Focus::Seekbar);
        app.dragging = Some(Drag::Seek);
        app.seek_dragging(seek_frac(r.seekbar, x));
        return;
    }

    // Album art (bottom-left) → open the large art popup.
    if hit(r.art, x, y) {
        app.open_art_popup();
        return;
    }

    if app.tab == Tab::Settings {
        return;
    }

    // Playlists tab: click a playlist or one of its tracks.
    if app.tab == Tab::Playlists {
        if hit(r.pl_list, x, y) {
            app.pl_set_pane(crate::app::PlPane::List);
            if let Some(i) = row_index(r.pl_list, &app.playlist_state, y, app.playlists.len(), 1) {
                app.playlist_state.select(Some(i));
                app.pl_move(0); // refresh track-pane selection
            }
        } else if hit(r.pl_tracks, x, y) {
            app.pl_set_pane(crate::app::PlPane::Tracks);
            let n = app.selected_playlist().map(|p| p.tracks.len()).unwrap_or(0);
            if let Some(i) = row_index(r.pl_tracks, &app.pl_track_state, y, n, 1) {
                app.pl_track_state.select(Some(i));
            }
        }
        return;
    }

    // Queue divider — grab to resize the tracks/queue split vertically.
    if near_queue_divider(&r, x, y) {
        app.set_focus(Focus::Queue);
        app.dragging = Some(Drag::QueueDivider);
        app.drag_queue_divider(y);
        return;
    }

    // Column divider — grab to resize.
    if let Some(i) = near_divider(&r, x, y) {
        app.dragging = Some(Drag::ColDivider(i));
        app.drag_divider(i, x);
        return;
    }

    // Search box.
    if hit(r.search, x, y) {
        app.set_focus(Focus::Search);
        return;
    }

    // Column rows.
    if hit(r.artists, x, y) {
        app.set_focus(Focus::Artists);
        if let Some(i) = row_index(r.artists, &app.artist_state, y, app.view_artists.len(), 1) {
            app.artist_state.select(Some(i));
            app.refresh_views();
        }
        if double {
            app.activate_play();
        }
        return;
    }
    if hit(r.albums, x, y) {
        app.set_focus(Focus::Albums);
        let row_h = if app.show_album_thumbnails { App::ALBUM_ROW_HEIGHT } else { 1 };
        if let Some(i) = row_index(r.albums, &app.album_state, y, app.view_albums.len(), row_h) {
            app.album_state.select(Some(i));
            app.refresh_views();
        }
        if double {
            app.activate_play();
        }
        return;
    }
    if hit(r.tracks, x, y) {
        app.set_focus(Focus::Tracks);
        if let Some(i) = row_index(r.tracks, &app.track_state, y, app.view_tracks.len(), 1) {
            app.track_state.select(Some(i));
        }
        if double {
            app.activate_play();
        }
        return;
    }
    if hit(r.queue, x, y) {
        app.set_focus(Focus::Queue);
        if let Some(i) = row_index(r.queue, &app.queue_state, y, app.queue.len(), 1) {
            app.queue_state.select(Some(i));
        }
        if double {
            app.activate_play();
        }
    }
}
