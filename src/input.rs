use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;

use crate::app::{App, Drag, Focus, PathField, PlPane, ScrollList, Tab};
use crate::keys::Action;
use crate::menu::Outcome;

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

    if app.confirm.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.confirm_answer(true),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => app.confirm_answer(false),
            _ => {}
        }
        return;
    }

    if app.goto_menu.is_some() {
        match key.code {
            // `g` again picks the highlighted entry, so `g g` is a quick radio.
            KeyCode::Enter | KeyCode::Char('g') => app.goto_choose(None),
            KeyCode::Esc | KeyCode::Char('q') => app.goto_menu = None,
            KeyCode::Up | KeyCode::Char('k') => app.goto_move(-1),
            KeyCode::Down | KeyCode::Char('j') => app.goto_move(1),
            KeyCode::Char(c) => app.goto_choose(Some(c)),
            _ => {}
        }
        return;
    }

    if let Some(menu) = &mut app.add_menu {
        if let Some(name) = &mut menu.naming {
            match key.code {
                KeyCode::Enter => app.add_menu_choose(),
                KeyCode::Esc => app.add_menu = None,
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) => name.push(c),
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Enter => app.add_menu_choose(),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => app.add_menu = None,
                KeyCode::Up | KeyCode::Char('k') => app.add_menu_move(-1),
                KeyCode::Down | KeyCode::Char('j') => app.add_menu_move(1),
                _ => {}
            }
        }
        return;
    }

    if let Some(buf) = &mut app.link_input {
        match key.code {
            KeyCode::Enter => app.commit_link_input(),
            KeyCode::Esc => app.link_input = None,
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
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
            KeyCode::Enter | KeyCode::Down => {
                app.set_focus(Focus::Artists);
                if app.tab == Tab::Streaming {
                    app.sp_set_pane(PlPane::Tracks);
                }
                if app.tab.browse() {
                    if key.code == KeyCode::Enter {
                        app.br_search_now();
                    }
                    app.br_set_pane(PlPane::List);
                }
            }
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

    // The right-click menu takes the arrows, Enter and Esc; any other key
    // closes it and does what it normally does.
    if let Some(menu) = &mut app.menu {
        let km = &app.keymap;
        let outcome = if km.is(&key, Action::Up) {
            menu.step(-1);
            Outcome::Stay
        } else if km.is(&key, Action::Down) {
            menu.step(1);
            Outcome::Stay
        } else if km.is(&key, Action::Activate) || km.is(&key, Action::Right) {
            menu.choose()
        } else if km.is(&key, Action::Left) || key.code == KeyCode::Esc {
            menu.back()
        } else if km.is(&key, Action::Menu) {
            Outcome::Close
        } else {
            app.menu = None;
            return handle_key(app, key);
        };
        return run_outcome(app, outcome);
    }

    if app.show_keys && key.code == KeyCode::Esc {
        app.show_keys = false;
        return;
    }

    // Settings rows also answer to their own letters.
    if app.tab == Tab::Settings && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
        let done = match key.code {
            KeyCode::Char(' ') => Some(app.settings_activate()),
            KeyCode::Char('e') => Some(app.begin_edit_path(PathField::MusicDir)),
            KeyCode::Char('p') => Some(app.begin_edit_path(PathField::SpotifyCsv)),
            KeyCode::Char('s') => Some(app.toggle_spotify()),
            KeyCode::Char('b') => Some(app.toggle_browser()),
            KeyCode::Char('r') => Some(app.start_scan()),
            KeyCode::Char('c') => Some(app.toggle_cache_all_art()),
            KeyCode::Char('t') => Some(app.toggle_sort_articles()),
            KeyCode::Char('v') => Some(app.toggle_album_thumbnails()),
            KeyCode::Char('+') | KeyCode::Char('=') => Some(app.adjust_gapless(0.5)),
            KeyCode::Char('-') | KeyCode::Char('_') => Some(app.adjust_gapless(-0.5)),
            _ => None,
        };
        if done.is_some() {
            return;
        }
    }

    // Everything else goes through the keymap; where a key has more than
    // one action, the first that applies here runs.
    let actions = app.keymap.actions(&key).to_vec();
    for action in actions {
        if app.run_action(action) {
            return;
        }
    }
}

/// Act on what the right-click menu returned.
fn run_outcome(app: &mut App, outcome: Outcome) {
    match outcome {
        Outcome::Stay => {}
        Outcome::Close => app.menu = None,
        Outcome::Run(cmd) => {
            app.menu = None;
            app.run_menu_cmd(cmd);
        }
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

/// The rightmost column of a list's rect — where `render_list` draws its
/// scrollbar thumb (see ui.rs). Dedicated to scrubbing rather than row
/// selection since it's a 1-cell gap left of any actual row content.
fn scrollbar_col(rect: Rect) -> u16 {
    rect.x + rect.width.saturating_sub(1)
}

/// Map a y position to a scroll offset proportional across the whole list —
/// used while a scrollbar thumb is being dragged. Bounded to
/// `0..=len.saturating_sub(visible)` so the ends of the track line up with
/// the very top/bottom of the list, matching where the thumb itself is drawn
/// (`render_list` in ui.rs). `row_h` is the text rows per item (2 for Albums).
fn scroll_offset_at(rect: Rect, y: u16, len: usize, row_h: u16) -> Option<usize> {
    if len == 0 || rect.height == 0 {
        return None;
    }
    let visible = ((rect.height / row_h.max(1)) as usize).max(1);
    let max_offset = len.saturating_sub(visible);
    if max_offset == 0 {
        return Some(0);
    }
    let span = (rect.height as f64 - 1.0).max(1.0);
    let rel = (y as i32 - rect.y as i32).clamp(0, span as i32) as f64;
    Some(((rel / span) * max_offset as f64).round() as usize)
}

/// Translate a mouse event into app state changes using the per-frame rects.
pub fn handle_mouse(app: &mut App, ev: MouseEvent) {
    let (x, y) = (ev.column, ev.row);
    if let Some(menu) = &mut app.menu {
        match ev.kind {
            MouseEventKind::Moved | MouseEventKind::Drag(_) => menu.hover(x, y),
            MouseEventKind::Down(MouseButton::Left) => {
                let outcome = menu.click(x, y);
                run_outcome(app, outcome);
            }
            MouseEventKind::Down(MouseButton::Right) if !menu.contains(x, y) => {
                // Right-click elsewhere: a new menu there.
                app.menu = None;
                return handle_mouse(app, ev);
            }
            MouseEventKind::ScrollDown => menu.step(1),
            MouseEventKind::ScrollUp => menu.step(-1),
            _ => {}
        }
        return;
    }
    match ev.kind {
        MouseEventKind::Down(MouseButton::Right) => {
            close_popups(app);
            if select_at(app, x, y) {
                app.open_context_menu((x, y));
            }
        }
        // Middle-click queues what's under the pointer.
        MouseEventKind::Down(MouseButton::Middle) => {
            close_popups(app);
            if select_at(app, x, y) {
                app.run_action(Action::Queue);
            }
        }
        MouseEventKind::ScrollDown if app.tab == Tab::Streaming && !hit(app.rects.volume, x, y) => {
            sp_scroll(app, x, y, 1)
        }
        MouseEventKind::ScrollUp if app.tab == Tab::Streaming && !hit(app.rects.volume, x, y) => {
            sp_scroll(app, x, y, -1)
        }
        MouseEventKind::ScrollDown if app.tab == Tab::Settings => app.settings_move(1),
        MouseEventKind::ScrollUp if app.tab == Tab::Settings => app.settings_move(-1),
        MouseEventKind::ScrollDown if app.tab.browse() && !hit(app.rects.volume, x, y) => {
            br_scroll(app, x, y, 1)
        }
        MouseEventKind::ScrollUp if app.tab.browse() && !hit(app.rects.volume, x, y) => {
            br_scroll(app, x, y, -1)
        }
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
                Some(Drag::SpDivider) => app.persist_sp_pct(),
                _ => {}
            }
            app.dragging = None;
        }
        _ => {}
    }
}

/// Wheel over a Spotify-tab pane: focus it and move its selection.
fn sp_scroll(app: &mut App, x: u16, y: u16, delta: i32) {
    if hit(app.rects.sp_list, x, y) {
        app.sp_set_pane(PlPane::List);
    } else if hit(app.rects.sp_tracks, x, y) {
        app.sp_set_pane(PlPane::Tracks);
    } else if hit(app.rects.queue, x, y) {
        app.sp_set_pane(PlPane::Queue);
    } else {
        return;
    }
    app.sp_move(delta);
}

/// Wheel over a Browser-tab pane: focus it and move its selection.
fn br_scroll(app: &mut App, x: u16, y: u16, delta: i32) {
    if hit(app.rects.br_sections, x, y) {
        app.br_set_pane(PlPane::List);
    } else if hit(app.rects.br_items, x, y) {
        app.br_set_pane(PlPane::Tracks);
    } else if hit(app.rects.queue, x, y) {
        app.br_set_pane(PlPane::Queue);
    } else {
        return;
    }
    app.br_move(delta);
}

/// While the left button is held, continue scrubbing whatever was grabbed on
/// mouse-down, even if the pointer strays off the bar.
fn handle_drag(app: &mut App, x: u16, y: u16) {
    match app.dragging {
        Some(Drag::Seek) => app.seek_dragging(seek_frac(app.rects.seekbar, x)),
        Some(Drag::Volume) => app.set_volume(vol_frac(app.rects.volume, y)),
        Some(Drag::ColDivider(i)) => app.drag_divider(i, x),
        Some(Drag::QueueDivider) => app.drag_queue_divider(y),
        Some(Drag::Scroll(target)) => scroll_drag(app, target, y),
        Some(Drag::SpDivider) => app.drag_sp_divider(x),
        None => {}
    }
}

/// Pan a list's viewport to the row under `y`, scaled across the whole list,
/// while a scrollbar thumb is being dragged. Never touches selection — see
/// `App::scroll_offset`.
fn scroll_drag(app: &mut App, target: ScrollList, y: u16) {
    let r = app.rects.clone();
    let hit = match target {
        ScrollList::Artists => scroll_offset_at(r.artists, y, app.view_artists.len(), 1),
        ScrollList::Albums => scroll_offset_at(r.albums, y, app.view_albums.len(), App::ALBUM_ROW_HEIGHT),
        ScrollList::Tracks => scroll_offset_at(r.tracks, y, app.view_tracks.len(), 1),
        ScrollList::Queue => scroll_offset_at(r.queue, y, app.queue.len(), 1),
        ScrollList::Settings => scroll_offset_at(r.settings, y, app.settings_rows().len(), 1),
        ScrollList::PlList => scroll_offset_at(r.pl_list, y, app.playlists.len(), 1),
        ScrollList::PlTracks => {
            let n = app.selected_playlist().map(|p| p.tracks.len()).unwrap_or(0);
            scroll_offset_at(r.pl_tracks, y, n, 1)
        }
        ScrollList::BrSections => scroll_offset_at(r.br_sections, y, app.br_section_count(), 1),
        ScrollList::BrItems => {
            let n = app.br_current().map(|v| v.items.len()).unwrap_or(0);
            scroll_offset_at(r.br_items, y, n, app.sp_row_h())
        }
        ScrollList::SpList => scroll_offset_at(r.sp_list, y, app.sp_view.len(), app.sp_row_h()),
        ScrollList::SpTracks => {
            let n = app.selected_sp_list().map(|p| p.tracks.len()).unwrap_or(0);
            scroll_offset_at(r.sp_tracks, y, n, app.sp_row_h())
        }
    };
    if let Some(offset) = hit {
        app.scroll_offset(target, offset);
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
    // A confirmation answers to its buttons; any other click cancels it.
    if app.confirm.is_some() {
        let yes = hit(app.rects.confirm_yes, x, y);
        app.confirm_answer(yes);
        return;
    }
    // A click while a popup is open just closes it.
    if app.goto_menu.take().is_some() {
        return;
    }
    // A fresh click ends any prior drag grab.
    app.dragging = None;
    let r = app.rects.clone();
    // Key hints run their action.
    if let Some(&(_, action)) = r.hints.iter().find(|(rect, _)| hit(*rect, x, y)) {
        app.run_action(action);
        return;
    }
    let double = app.is_double_click(x, y);

    // Tab bar.
    if let Some(&(tab, _)) = r.tabs.iter().find(|(_, rect)| hit(*rect, x, y)) {
        app.set_tab(tab);
        return;
    }
    // "? keys" in the top-right.
    if hit(r.help_hint, x, y) {
        app.show_keys = !app.show_keys;
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
        let r = app.rects.settings;
        let n = app.settings_rows().len();
        if hit(r, x, y) && x == scrollbar_col(r) {
            app.dragging = Some(Drag::Scroll(ScrollList::Settings));
            if let Some(offset) = scroll_offset_at(r, y, n, 1) {
                app.scroll_offset(ScrollList::Settings, offset);
            }
            return;
        }
        if hit(r, x, y)
            && let Some(i) = row_index(r, &app.settings_state, y, n, 1)
            && app.settings_rows()[i].selectable()
        {
            // Click selects; clicking the selected row again acts on it.
            if app.settings_state.selected() == Some(i) || double {
                app.settings_activate();
            } else {
                app.settings_state.select(Some(i));
            }
        }
        return;
    }

    // Browser tab: click a section or row; double-click plays/opens.
    if app.tab.browse() {
        if hit(r.search, x, y) {
            app.set_focus(Focus::Search);
            return;
        }
        let row_h = app.sp_row_h();
        let b = r.sp_body;
        let on_div = r.sp_div > 0 && (x == r.sp_div || x == r.sp_div - 1);
        if on_div && y >= b.y && y < b.y + b.height {
            app.dragging = Some(Drag::SpDivider);
            return;
        }
        if near_queue_divider(&r, x, y) {
            app.dragging = Some(Drag::QueueDivider);
            app.drag_queue_divider(y);
            return;
        }
        if hit(r.queue, x, y) {
            app.br_set_pane(PlPane::Queue);
            if x == scrollbar_col(r.queue) {
                app.dragging = Some(Drag::Scroll(ScrollList::Queue));
                if let Some(offset) = scroll_offset_at(r.queue, y, app.queue.len(), 1) {
                    app.scroll_offset(ScrollList::Queue, offset);
                }
                return;
            }
            if let Some(i) = row_index(r.queue, &app.queue_state, y, app.queue.len(), 1) {
                app.queue_state.select(Some(i));
                if double {
                    app.queue_play_selected();
                }
            }
            return;
        }
        if hit(r.br_sections, x, y) {
            app.br_set_pane(PlPane::List);
            let n = app.br_section_count();
            if x == scrollbar_col(r.br_sections) {
                app.dragging = Some(Drag::Scroll(ScrollList::BrSections));
                if let Some(offset) = scroll_offset_at(r.br_sections, y, n, 1) {
                    app.scroll_offset(ScrollList::BrSections, offset);
                }
                return;
            }
            if let Some(i) = row_index(r.br_sections, &app.br_sec_state, y, n, 1) {
                app.br_sec_state.select(Some(i));
                app.br_move(0);
            }
        } else if hit(r.br_items, x, y) {
            app.br_set_pane(PlPane::Tracks);
            let n = app.br_current().map(|v| v.items.len()).unwrap_or(0);
            if x == scrollbar_col(r.br_items) {
                app.dragging = Some(Drag::Scroll(ScrollList::BrItems));
                if let Some(offset) = scroll_offset_at(r.br_items, y, n, row_h) {
                    app.scroll_offset(ScrollList::BrItems, offset);
                }
                return;
            }
            if let Some(i) = row_index(r.br_items, &app.br_item_state, y, n, row_h) {
                app.br_item_state.select(Some(i));
                if double {
                    app.br_activate();
                }
            }
        }
        return;
    }

    // Spotify tab: click a list or one of its tracks; double-click plays.
    if app.tab == Tab::Streaming {
        if hit(r.search, x, y) {
            app.set_focus(Focus::Search);
            return;
        }
        let row_h = app.sp_row_h();
        let b = r.sp_body;
        let on_div = r.sp_div > 0 && (x == r.sp_div || x == r.sp_div - 1);
        if on_div && y >= b.y && y < b.y + b.height {
            app.dragging = Some(Drag::SpDivider);
            return;
        }
        if near_queue_divider(&r, x, y) {
            app.dragging = Some(Drag::QueueDivider);
            app.drag_queue_divider(y);
            return;
        }
        if hit(r.queue, x, y) {
            app.sp_set_pane(PlPane::Queue);
            if x == scrollbar_col(r.queue) {
                app.dragging = Some(Drag::Scroll(ScrollList::Queue));
                if let Some(offset) = scroll_offset_at(r.queue, y, app.queue.len(), 1) {
                    app.scroll_offset(ScrollList::Queue, offset);
                }
                return;
            }
            if let Some(i) = row_index(r.queue, &app.queue_state, y, app.queue.len(), 1) {
                app.queue_state.select(Some(i));
                if double {
                    app.queue_play_selected();
                }
            }
            return;
        }
        if hit(r.sp_list, x, y) {
            app.sp_set_pane(PlPane::List);
            if x == scrollbar_col(r.sp_list) {
                app.dragging = Some(Drag::Scroll(ScrollList::SpList));
                if let Some(offset) = scroll_offset_at(r.sp_list, y, app.sp_view.len(), row_h) {
                    app.scroll_offset(ScrollList::SpList, offset);
                }
                return;
            }
            if let Some(i) = row_index(r.sp_list, &app.sp_list_state, y, app.sp_view.len(), row_h) {
                app.sp_list_state.select(Some(i));
                app.sp_move(0);
            }
        } else if hit(r.sp_tracks, x, y) {
            app.sp_set_pane(PlPane::Tracks);
            let n = app.selected_sp_list().map(|p| p.tracks.len()).unwrap_or(0);
            if x == scrollbar_col(r.sp_tracks) {
                app.dragging = Some(Drag::Scroll(ScrollList::SpTracks));
                if let Some(offset) = scroll_offset_at(r.sp_tracks, y, n, row_h) {
                    app.scroll_offset(ScrollList::SpTracks, offset);
                }
                return;
            }
            if let Some(i) = row_index(r.sp_tracks, &app.sp_track_state, y, n, row_h) {
                app.sp_track_state.select(Some(i));
                if double {
                    app.sp_activate();
                }
            }
        }
        return;
    }

    // Playlists tab: click a playlist or one of its tracks.
    if app.tab == Tab::Playlists {
        if hit(r.pl_list, x, y) {
            app.pl_set_pane(crate::app::PlPane::List);
            if x == scrollbar_col(r.pl_list) {
                app.dragging = Some(Drag::Scroll(ScrollList::PlList));
                if let Some(offset) = scroll_offset_at(r.pl_list, y, app.playlists.len(), 1) {
                    app.scroll_offset(ScrollList::PlList, offset);
                }
                return;
            }
            if let Some(i) = row_index(r.pl_list, &app.playlist_state, y, app.playlists.len(), 1) {
                app.playlist_state.select(Some(i));
                app.pl_move(0); // refresh track-pane selection
            }
        } else if hit(r.pl_tracks, x, y) {
            app.pl_set_pane(crate::app::PlPane::Tracks);
            let n = app.selected_playlist().map(|p| p.tracks.len()).unwrap_or(0);
            if x == scrollbar_col(r.pl_tracks) {
                app.dragging = Some(Drag::Scroll(ScrollList::PlTracks));
                if let Some(offset) = scroll_offset_at(r.pl_tracks, y, n, 1) {
                    app.scroll_offset(ScrollList::PlTracks, offset);
                }
                return;
            }
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
        if x == scrollbar_col(r.artists) {
            app.dragging = Some(Drag::Scroll(ScrollList::Artists));
            if let Some(offset) = scroll_offset_at(r.artists, y, app.view_artists.len(), 1) {
                app.scroll_offset(ScrollList::Artists, offset);
            }
            return;
        }
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
        if x == scrollbar_col(r.albums) {
            app.dragging = Some(Drag::Scroll(ScrollList::Albums));
            if let Some(offset) = scroll_offset_at(r.albums, y, app.view_albums.len(), App::ALBUM_ROW_HEIGHT) {
                app.scroll_offset(ScrollList::Albums, offset);
            }
            return;
        }
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
        if x == scrollbar_col(r.tracks) {
            app.dragging = Some(Drag::Scroll(ScrollList::Tracks));
            if let Some(offset) = scroll_offset_at(r.tracks, y, app.view_tracks.len(), 1) {
                app.scroll_offset(ScrollList::Tracks, offset);
            }
            return;
        }
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
        if x == scrollbar_col(r.queue) {
            app.dragging = Some(Drag::Scroll(ScrollList::Queue));
            if let Some(offset) = scroll_offset_at(r.queue, y, app.queue.len(), 1) {
                app.scroll_offset(ScrollList::Queue, offset);
            }
            return;
        }
        if let Some(i) = row_index(r.queue, &app.queue_state, y, app.queue.len(), 1) {
            app.queue_state.select(Some(i));
        }
        if double {
            app.activate_play();
        }
    }
}

/// Close the overlays a click would close.
fn close_popups(app: &mut App) {
    app.show_help = false;
    app.show_art_popup = false;
    app.goto_menu = None;
    app.confirm = None;
}

/// Highlight (and focus) the list row under the pointer, as a click would
/// but without playing or grabbing anything. False if there's no row there.
fn select_at(app: &mut App, x: u16, y: u16) -> bool {
    let r = app.rects.clone();
    let row_h = app.sp_row_h();
    let hit_row = |rect: Rect, state: &ListState, len: usize, h: u16| {
        if hit(rect, x, y) && x != scrollbar_col(rect) { row_index(rect, state, y, len, h) } else { None }
    };
    if r.queue.width > 0 && matches!(app.tab, Tab::Local | Tab::Streaming) || r.queue.width > 0 && app.tab.browse() {
        if let Some(i) = hit_row(r.queue, &app.queue_state, app.queue.len(), 1) {
            match app.tab {
                Tab::Local => app.set_focus(Focus::Queue),
                Tab::Streaming => app.sp_set_pane(PlPane::Queue),
                _ => app.br_set_pane(PlPane::Queue),
            }
            app.queue_state.select(Some(i));
            return true;
        }
    }
    match app.tab {
        Tab::Local => {
            let album_h = if app.show_album_thumbnails { App::ALBUM_ROW_HEIGHT } else { 1 };
            if let Some(i) = hit_row(r.artists, &app.artist_state, app.view_artists.len(), 1) {
                app.set_focus(Focus::Artists);
                app.artist_state.select(Some(i));
                app.refresh_views();
            } else if let Some(i) = hit_row(r.albums, &app.album_state, app.view_albums.len(), album_h) {
                app.set_focus(Focus::Albums);
                app.album_state.select(Some(i));
                app.refresh_views();
            } else if let Some(i) = hit_row(r.tracks, &app.track_state, app.view_tracks.len(), 1) {
                app.set_focus(Focus::Tracks);
                app.track_state.select(Some(i));
            } else {
                return false;
            }
        }
        Tab::Streaming => {
            let tracks = app.selected_sp_list().map(|p| p.tracks.len()).unwrap_or(0);
            if let Some(i) = hit_row(r.sp_list, &app.sp_list_state, app.sp_view.len(), row_h) {
                app.sp_set_pane(PlPane::List);
                if app.sp_list_state.selected() != Some(i) {
                    app.sp_list_state.select(Some(i));
                    app.sp_move(0);
                }
            } else if let Some(i) = hit_row(r.sp_tracks, &app.sp_track_state, tracks, row_h) {
                app.sp_set_pane(PlPane::Tracks);
                app.sp_track_state.select(Some(i));
            } else {
                return false;
            }
        }
        Tab::Browser | Tab::Radio | Tab::Podcasts => {
            let items = app.br_current().map(|v| v.items.len()).unwrap_or(0);
            if let Some(i) = hit_row(r.br_sections, &app.br_sec_state, app.br_section_count(), 1) {
                app.br_set_pane(PlPane::List);
                if app.br_sec_state.selected() != Some(i) {
                    app.br_sec_state.select(Some(i));
                    app.br_move(0);
                }
            } else if let Some(i) = hit_row(r.br_items, &app.br_item_state, items, row_h) {
                app.br_set_pane(PlPane::Tracks);
                app.br_item_state.select(Some(i));
            } else {
                return false;
            }
        }
        Tab::Playlists => {
            let tracks = app.selected_playlist().map(|p| p.tracks.len()).unwrap_or(0);
            if let Some(i) = hit_row(r.pl_list, &app.playlist_state, app.playlists.len(), 1) {
                app.pl_set_pane(PlPane::List);
                app.playlist_state.select(Some(i));
                app.pl_move(0);
            } else if let Some(i) = hit_row(r.pl_tracks, &app.pl_track_state, tracks, 1) {
                app.pl_set_pane(PlPane::Tracks);
                app.pl_track_state.select(Some(i));
            } else {
                return false;
            }
        }
        Tab::Settings => return false,
    }
    true
}
