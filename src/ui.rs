use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, BorderType, Borders, Clear, Gauge, List, ListItem, Paragraph,
};
use crossterm::event::KeyCode;
use ratatui::Frame;
use ratatui_image::{Resize, StatefulImage};

use crate::app::{App, Focus, PathField, PlPane, Tab};
use crate::config::Theme;
use crate::model::{display_artist, fmt_time};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Length(3), // search
        Constraint::Min(5),    // body
        Constraint::Length(1), // key hints
        Constraint::Length(3), // seek bar
        Constraint::Length(7), // art + transport
    ])
    .split(f.area());

    // The queue's rects are set again by whichever tab draws it.
    app.rects.queue = Rect::default();
    app.rects.col3 = Rect::default();
    app.rects.queue_div = 0;
    draw_tabs(f, app, chunks[0]);
    draw_search(f, app, chunks[1]);
    match app.tab {
        Tab::Local => draw_body(f, app, chunks[2]),
        Tab::Streaming => draw_spotify(f, app, chunks[2]),
        Tab::Browser => draw_browser(f, app, chunks[2]),
        Tab::Playlists => draw_playlists(f, app, chunks[2]),
        Tab::Settings => draw_library(f, app, chunks[2]),
    }
    draw_hints(f, app, chunks[3]);
    draw_seekbar(f, app, chunks[4]);
    draw_bottom(f, app, chunks[5]);

    if app.add_menu.is_some() {
        draw_add_menu(f, app, chunks[2]);
    }
    if app.goto_menu.is_some() {
        draw_goto_menu(f, app, chunks[2]);
    }
    if app.confirm.is_some() {
        draw_confirm(f, app, chunks[2]);
    }
    if let Some(buf) = &app.link_input {
        let t = app.theme;
        let area = chunks[2];
        let w = 72.min(area.width.saturating_sub(4));
        let popup = Rect::new(area.x + (area.width - w) / 2, area.y + area.height / 2, w, 3);
        f.render_widget(Clear, popup);
        let line = Line::from(vec![
            Span::styled(buf.clone(), Style::default().fg(t.accent)),
            Span::styled("█", Style::default().fg(t.accent)),
        ]);
        let block = Block::bordered().border_type(BorderType::Rounded)
            .title(" Paste a Spotify playlist link (Enter / Esc) ")
            .border_style(Style::default().fg(t.accent));
        f.render_widget(Paragraph::new(line).block(block), popup);
    }
    if app.show_help {
        draw_help(f, f.area(), app.tab, app.theme);
    }
    if app.show_art_popup {
        draw_art_popup(f, app);
    }
}

/// A large, centered album-art overlay. The cover is square, so the inner region
/// is made square *in pixels* from the terminal's actual cell size (cells are
/// taller than wide) to minimize letterboxing. A dedicated protocol is used (see
/// `App::open_art_popup`) so the cover isn't rendered to two areas in one frame.
fn draw_art_popup(f: &mut Frame, app: &mut App) {
    let t = app.theme;
    let area = f.area();
    // Cell pixel size (width, height); fall back to a typical 1:2 cell.
    let (cw, ch) = app
        .art
        .font_size()
        .map(|fs| (fs.width.max(1) as f64, fs.height.max(1) as f64))
        .unwrap_or((1.0, 2.0));
    // Size the box to the cover's own aspect ratio (covers aren't always square),
    // bounded by the available space and a pixel cap that keeps the kitty transmit
    // small (a full-screen cover is several MB and slow to encode/send).
    const MAX_POPUP_PX: f64 = 1024.0;
    let (iw_px, ih_px) = app.popup_art_size().unwrap_or((1, 1));
    let (iw_px, ih_px) = (iw_px.max(1) as f64, ih_px.max(1) as f64);
    let avail_w = (area.width.saturating_sub(6) as f64) * cw;
    let avail_h = (area.height.saturating_sub(6) as f64) * ch;
    let s = (avail_w / iw_px)
        .min(avail_h / ih_px)
        .min(MAX_POPUP_PX / iw_px.max(ih_px));
    // Ceil (matching ratatui-image's own cell rounding) so the box is the image's
    // exact cell footprint. The kitty renderer anchors the image top-left and fills
    // only min(area, footprint), so a box even slightly taller leaves the slack at
    // the bottom — i.e. the border hanging below the art. Ceiling collapses it.
    let iw = ((iw_px * s) / cw).ceil().max(1.0) as u16;
    let ih = ((ih_px * s) / ch).ceil().max(1.0) as u16;
    let w = iw + 2;
    let h = ih + 2;
    let popup = Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, popup);
    let block = Block::bordered().border_type(BorderType::Rounded)
        .title(" Album art (any key to close) ")
        .border_style(Style::default().fg(t.accent));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    if let Some(proto) = app.art_popup_proto.as_mut() {
        // Scale (unlike Fit) upscales to fill the region; with a square box and a
        // square cover this fills edge-to-edge even for low-res art. Triangle is
        // ~3x faster to encode than Lanczos3 and still smooth for an upscale.
        let img = StatefulImage::default()
            .resize(Resize::Scale(Some(ratatui_image::FilterType::Triangle)));
        f.render_stateful_widget(img, inner, proto);
    } else {
        f.render_widget(
            Paragraph::new("\n♪  No album art")
                .alignment(Alignment::Center)
                .fg(t.muted),
            inner,
        );
    }
}

fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    // Render each tab as its own span (so clicks map to actual glyph positions)
    // with a │ separator between them.
    let tabs = app.tabs();
    let mut rects = Vec::with_capacity(tabs.len());
    let mut x = area.x;
    for (i, &tab) in tabs.iter().enumerate() {
        let label = format!(" ({}) {} ", i + 1, tab.label());
        if i > 0 {
            f.render_widget(
                Paragraph::new(Span::styled("│", Style::default().fg(t.border))),
                Rect::new(x, area.y, 1, 1),
            );
            x += 1;
        }
        let w = label.chars().count() as u16;
        let r = Rect::new(x, area.y, w, 1);
        rects.push((tab, r));
        let style = if app.tab == tab {
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.muted)
        };
        f.render_widget(Paragraph::new(Span::styled(label, style)), r);
        x += w;
    }
    app.rects.tabs = rects;

    let hint_str = "music-tui  ·  ? help ";
    let hw = hint_str.chars().count() as u16;
    let hint = Line::from(hint_str).right_aligned().fg(t.muted);
    f.render_widget(Paragraph::new(hint), area);
    app.rects.help_hint = Rect::new(
        area.x + area.width.saturating_sub(hw),
        area.y,
        hw.min(area.width),
        1,
    );
}

/// The keys that matter for the focused pane, in words. `None` keys are
/// shown but not clickable.
fn hints(app: &App) -> Vec<(&'static str, Option<KeyCode>, &'static str)> {
    let c = |ch| Some(KeyCode::Char(ch));
    if !app.panes_focused() {
        return Vec::new();
    }
    let queue_hint = ("Q", c('Q'), if app.config.ui.show_queue { "Hide queue" } else { "Show queue" });
    let mut h: Vec<(&'static str, Option<KeyCode>, &'static str)> = match app.tab {
        _ if app.queue_focused() => vec![
            ("d", c('d'), "Remove"),
            ("J/K", None, "Move"),
            ("c", c('c'), "Clear queue"),
            ("g", c('g'), "Go to…"),
        ],
        Tab::Local => vec![
            ("a", c('a'), "Add to queue"),
            ("g", c('g'), "Go to… (radio, artist)"),
            ("/", c('/'), "Search"),
        ],
        Tab::Streaming if app.sp_focus == PlPane::List => vec![
            ("a", c('a'), "Queue list"),
            ("m", c('m'), "Add list to…"),
            ("u", c('u'), "Link a playlist"),
            ("d", c('d'), "Remove"),
            ("/", c('/'), "Search"),
        ],
        Tab::Streaming => vec![
            ("g", c('g'), "Go to… (radio, artist, album)"),
            ("f", c('f'), "Like"),
            ("m", c('m'), "Add to playlist"),
            ("a", c('a'), "Queue"),
            ("x", c('x'), "Wrong song?"),
            ("o", c('o'), "Sort"),
            ("/", c('/'), "Search"),
        ],
        Tab::Browser if app.br_focus == PlPane::List => vec![
            ("a", c('a'), "Queue all"),
            ("S", c('S'), "Save as playlist"),
            ("/", c('/'), "Search"),
        ],
        Tab::Browser => {
            let mut v = vec![
                ("g", c('g'), "Go to… (radio, artist, album)"),
                ("f", c('f'), "Like"),
                ("m", c('m'), "Add to playlist"),
                ("a", c('a'), "Queue"),
                ("S", c('S'), "Save list"),
                ("x", c('x'), "Wrong song?"),
            ];
            if !app.br_stack.is_empty() {
                v.insert(0, ("⌫", Some(KeyCode::Backspace), "Back"));
            }
            v
        }
        Tab::Playlists => vec![
            ("n", c('n'), "New from queue"),
            ("a", c('a'), "Append queue"),
            ("d", c('d'), "Delete list"),
            ("x", c('x'), "Remove song"),
        ],
        Tab::Settings => Vec::new(),
    };
    if matches!(app.tab, Tab::Local | Tab::Streaming | Tab::Browser) {
        h.push(queue_hint);
    }
    h
}

fn draw_hints(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let mut all = hints(app);
    // Status on the right: what's running in the background.
    let mut right = Vec::new();
    if let Some(r) = &app.br_radio_building {
        right.push(Span::styled(format!("◌ building {r}…  "), Style::default().fg(t.accent)));
    }
    if app.config.playback.keep_going {
        right.push(Span::styled("↻ keep going  ", Style::default().fg(t.accent)));
    }
    let right_w: u16 = right.iter().map(|s| s.content.chars().count() as u16).sum();
    let room = area.width.saturating_sub(right_w + 1);
    let width = |h: &(&str, Option<KeyCode>, &str)| (h.0.chars().count() + h.2.chars().count() + 4) as u16;
    // Drop hints from the end until they fit.
    while !all.is_empty() && all.iter().map(width).sum::<u16>() + 1 > room {
        all.pop();
    }
    let mut spans = vec![Span::raw(" ")];
    let mut x = area.x + 1;
    app.rects.hints.clear();
    for h in &all {
        let w = width(h);
        if let Some(code) = h.1 {
            app.rects.hints.push((Rect::new(x, area.y, w - 2, 1), code));
        }
        spans.push(Span::styled(format!(" {} ", h.0), Style::default().fg(t.selection_fg).bg(t.accent)));
        spans.push(Span::styled(format!(" {}", h.2), Style::default().fg(t.muted)));
        spans.push(Span::raw("  "));
        x += w;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    f.render_widget(Paragraph::new(Line::from(right)).alignment(Alignment::Right), area);
}

fn draw_goto_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let entries = app.goto_entries();
    let Some(menu) = &app.goto_menu else { return };
    let w = 60.min(area.width.saturating_sub(4));
    let h = (entries.len() as u16 + 2).min(area.height);
    let popup = Rect::new(area.x + area.width.saturating_sub(w) / 2, area.y + area.height.saturating_sub(h) / 2, w, h);
    f.render_widget(Clear, popup);
    let items: Vec<ListItem> = entries
        .iter()
        .map(|(k, label, _)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {k} "), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
                Span::raw(fit(label, w.saturating_sub(7) as usize)),
            ]))
        })
        .collect();
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(menu.sel));
    let list = List::new(items)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" Go to… · {} ", fit(&menu.track.title, w.saturating_sub(14) as usize)))
                .title_bottom(Line::from(" Enter choose · Esc close ").right_aligned())
                .border_style(Style::default().fg(t.accent)),
        )
        .highlight_style(Style::default().bg(t.accent).fg(t.selection_fg));
    f.render_stateful_widget(list, popup, &mut state);
}

fn draw_confirm(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let Some(c) = &app.confirm else { return };
    let w = 64.min(area.width.saturating_sub(4));
    let inner = w.saturating_sub(4) as usize;
    let popup = Rect::new(area.x + area.width.saturating_sub(w) / 2, area.y + area.height.saturating_sub(6) / 2, w, 6);
    f.render_widget(Clear, popup);
    let chip = |k: &str| Span::styled(format!(" {k} "), Style::default().fg(t.selection_fg).bg(t.accent));
    let text = Text::from(vec![
        Line::from(Span::styled(fit(&c.subject, inner), Style::default().add_modifier(Modifier::BOLD))),
        Line::from(Span::styled(fit(&c.detail, inner), Style::default().fg(t.muted))),
        Line::from(""),
        Line::from(vec![chip("y"), Span::raw(format!(" {}    ", c.yes)), chip("n"), Span::raw(format!(" {}", c.no))]),
    ]);
    let block = Block::bordered().border_type(BorderType::Rounded).title(c.title).border_style(Style::default().fg(t.accent));
    f.render_widget(Paragraph::new(text).block(block), popup);
}

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let focused = app.focus == Focus::Search;
    // The box title and the placeholder share one style so they read as a
    // pair; the placeholder says what's searched.
    let hint = match app.tab {
        Tab::Browser => "Search all music (Deezer · YouTube Music) — click or press /",
        Tab::Streaming => "Search your Spotify library and playlists — click or press /",
        _ => "Search title / artist / album — click or press /",
    };
    let block = titled(" Search ", focused, t);
    let inner = block.inner(area);
    app.rects.search = inner;
    let mut spans = vec![Span::raw(" ")];
    if app.search.is_empty() && !focused {
        spans.push(Span::styled(hint, Style::default().fg(t.muted)));
    } else if !focused {
        spans.push(Span::raw(&app.search));
    } else {
        // Split the text at the cursor; the character under the cursor is drawn
        // reversed as a block, or a trailing block when the cursor is at the end.
        let chars: Vec<char> = app.search.chars().collect();
        let cur = app.search_cursor.min(chars.len());
        spans.push(Span::raw(chars[..cur].iter().collect::<String>()));
        if cur < chars.len() {
            spans.push(Span::raw(chars[cur].to_string()).reversed());
            spans.push(Span::raw(chars[cur + 1..].iter().collect::<String>()));
        } else {
            spans.push(Span::raw("█").fg(t.accent));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let [w0, w1, w2] = app.col_widths;
    let cols = Layout::horizontal([
        Constraint::Percentage(w0),
        Constraint::Percentage(w1),
        Constraint::Percentage(w2),
    ])
    .split(area);
    // Record the body region and the two divider x positions for resize drags.
    app.rects.body = area;
    app.rects.col_div = [cols[1].x, cols[2].x];

    // Column 1: album artists.
    let sort_articles = app.config.ui.sort_articles;
    let items: Vec<ListItem> = app
        .view_artists
        .iter()
        .map(|a| ListItem::new(display_artist(a, sort_articles)))
        .collect();
    render_list(
        f,
        &mut app.artist_state,
        items,
        cols[0],
        " Album Artists ",
        app.focus == Focus::Artists,
        &mut app.rects.artists,
        t,
        1,
    );

    // Column 2: albums by year. When thumbnails are on, each row grows to two
    // lines (cover + title, blank spacer) and reserves `ALBUM_THUMB_COLS` on the
    // left for the image, drawn separately in `draw_album_thumbnails` after this
    // list (List/ListItem only draws text).
    let show_thumbs = app.show_album_thumbnails;
    let indent = " ".repeat(ALBUM_THUMB_COLS as usize + 1);
    let items: Vec<ListItem> = app
        .view_albums
        .iter()
        .enumerate()
        .map(|(i, al)| {
            let mut spans = if show_thumbs {
                vec![Span::raw(indent.clone())]
            } else {
                Vec::new()
            };
            if i == 0 {
                spans.push(Span::raw("All"));
            } else {
                let year = al.year.map(|y| y.to_string()).unwrap_or_else(|| "----".into());
                spans.push(Span::styled(format!("[{year}] "), Style::default().fg(t.muted)));
                spans.push(Span::raw(strip_year_bracket(&al.title).to_string()));
            }
            let line = Line::from(spans);
            if show_thumbs {
                ListItem::new(Text::from(vec![line, Line::raw("")]))
            } else {
                ListItem::new(line)
            }
        })
        .collect();
    render_list(
        f,
        &mut app.album_state,
        items,
        cols[1],
        " Albums ",
        app.focus == Focus::Albums,
        &mut app.rects.albums,
        t,
        if show_thumbs { App::ALBUM_ROW_HEIGHT } else { 1 },
    );
    if show_thumbs {
        draw_album_thumbnails(f, app);
    }

    // Column 3: tracks (top) + queue (resizable bottom share).
    let (songs, queue) = split_queue(app, cols[2]);
    let right = [songs, queue.unwrap_or_default()];

    let inner_w = right[0].width.saturating_sub(2) as usize;
    // Layout: "<no> <title>  <album> … <length> " — length is pinned right (a
    // fixed numeric field reads cleanest at the edge, and no longer interrupts
    // the text flow). Title and album share the remaining width sized to the
    // longest value *in the current view*: when they fit, each takes exactly
    // what it needs (so album never truncates while there's room); when they
    // don't, they shrink proportionally.
    let max_title = app.view_tracks.iter().map(|t| t.title.chars().count()).max().unwrap_or(1).max(1);
    let max_album = app.view_tracks.iter().map(|t| t.album.chars().count()).max().unwrap_or(1).max(1);
    // Length field width = widest time in the view (5 normally, 7 for >1h tracks).
    let len_w = app.view_tracks.iter()
        .map(|t| fmt_time(t.length_secs).chars().count())
        .max().unwrap_or(5).max(4);
    // Space for title+album plus the gap before the length field.
    // Fixed cells: 4 (no) + 2 (inter-column gap) + len_w + 1 (trailing).
    let avail = inner_w.saturating_sub(7 + len_w);
    let want = max_title + max_album;
    let budget = avail.saturating_sub(1); // keep ≥1 for the gap before length
    let (title_w, album_w) = if want <= budget {
        (max_title, max_album)
    } else {
        let tw = (budget * max_title / want).max(1);
        let aw = budget.saturating_sub(tw).max(1);
        (tw, aw)
    };
    let items: Vec<ListItem> = app
        .view_tracks
        .iter()
        .map(|tr| {
            let no = tr.track_no.map(|n| n.to_string()).unwrap_or_default();
            let len = fmt_time(tr.length_secs);
            // Gap before length absorbs all slack so the row fills inner_w exactly.
            let used = 4 + title_w + 2 + album_w + len_w + 1;
            let fill = inner_w.saturating_sub(used).max(1);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{no:>3} "), Style::default().fg(t.muted)),
                Span::raw(fit(&tr.title, title_w)),
                Span::raw("  "),
                Span::styled(fit(&tr.album, album_w), Style::default().fg(t.muted)),
                Span::raw(" ".repeat(fill)),
                Span::styled(format!("{len:>len_w$} "), Style::default().fg(t.muted)),
            ]))
        })
        .collect();
    render_list(
        f,
        &mut app.track_state,
        items,
        right[0],
        " # · Title · Album · Length ",
        app.focus == Focus::Tracks,
        &mut app.rects.tracks,
        t,
        1,
    );

    if let Some(q) = queue {
        let focused = app.focus == Focus::Queue;
        draw_queue(f, app, q, focused);
    }
}

/// Split a songs column into songs (top) and the queue (bottom share), or
/// leave it whole when the queue is hidden.
fn split_queue(app: &mut App, area: Rect) -> (Rect, Option<Rect>) {
    if !app.config.ui.show_queue {
        return (area, None);
    }
    let qp = app.queue_pct;
    let parts = Layout::vertical([Constraint::Percentage(100 - qp), Constraint::Percentage(qp)]).split(area);
    app.rects.col3 = area;
    app.rects.queue_div = parts[1].y;
    (parts[0], Some(parts[1]))
}

fn draw_queue(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let t = app.theme;
    let now = app.now;
    let items: Vec<ListItem> = app
        .queue
        .iter()
        .enumerate()
        .map(|(i, tr)| {
            let marker = if Some(i) == now { "▶ " } else { "  " };
            let style = if Some(i) == now { Style::default().fg(t.accent) } else { Style::default() };
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(format!("{} — {}", tr.title, tr.track_artist), style),
            ]))
        })
        .collect();
    let title = if app.queue.is_empty() { " Queue (empty) ".to_string() } else { format!(" Queue · {} ", app.queue.len()) };
    render_list(f, &mut app.queue_state, items, area, &title, focused, &mut app.rects.queue, t, 1);
}

/// Columns reserved at the left of each Albums row for its cover thumbnail.
const ALBUM_THUMB_COLS: u16 = 4;

/// Overlays a small cover image on each Albums-column row currently scrolled
/// into view (the `List` widget itself only draws text). Only draws rows
/// `App::request_visible_row_thumbs` would also consider in-view, and skips
/// any row whose thumbnail hasn't finished decoding yet — it simply shows
/// blank until the next frame it's ready, same as the now-playing cover does.
fn draw_album_thumbnails(f: &mut Frame, app: &mut App) {
    let area = app.rects.albums;
    let offset = app.album_state.offset();
    draw_row_thumbs(f, area, offset, &app.view_albums, &mut app.row_thumb_cache);
}

/// Draw the cached thumbnail (if decoded yet) for each `ALBUM_ROW_HEIGHT`-tall
/// row of `area` in view, keyed by `keys[row]`.
fn draw_row_thumbs(
    f: &mut Frame,
    area: Rect,
    offset: usize,
    keys: &[crate::model::AlbumRef],
    cache: &mut std::collections::HashMap<crate::model::AlbumRef, ratatui_image::protocol::StatefulProtocol>,
) {
    let row_h = App::ALBUM_ROW_HEIGHT;
    if area.height < row_h || area.width <= ALBUM_THUMB_COLS {
        return;
    }
    let visible = (area.height / row_h) as usize;
    let end = (offset + visible).min(keys.len());
    for row_i in offset..end {
        let Some(proto) = cache.get_mut(&keys[row_i]) else {
            continue;
        };
        let y = area.y + ((row_i - offset) as u16) * row_h;
        let rect = Rect::new(area.x, y, ALBUM_THUMB_COLS, row_h);
        let img = StatefulImage::default().resize(Resize::Fit(None));
        f.render_stateful_widget(img, rect, proto);
    }
}

fn draw_library(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::app::SettingRow::*;
    let t = app.theme;
    let on_off = |b: bool| if b { "● on" } else { "○ off" };
    let home = directories::UserDirs::new().map(|u| u.home_dir().to_path_buf());
    let tilde = |p: &std::path::Path| -> String {
        match home.as_deref().and_then(|h| p.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => p.display().to_string(),
        }
    };
    let size = |b: u64| -> String {
        match b {
            b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
            b if b >= 1 << 20 => format!("{:.0} MB", b as f64 / (1u64 << 20) as f64),
            b => format!("{} KB", b / 1024),
        }
    };
    let (audio, sp_covers, art) = app.cache_stats.unwrap_or_default();
    let editing = |field: PathField| app.editing_path.as_ref().filter(|_| app.editing_field == field);

    let rows = app.settings_rows();
    let label_w = 26;
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            // (label, value, value is accent-colored)
            let (label, value, strong): (&str, String, bool) = match *row {
                Header(h) => {
                    return ListItem::new(Line::from(Span::styled(
                        format!(" {h}"),
                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                    )));
                }
                MusicDir => (
                    "Local music dir",
                    editing(PathField::MusicDir)
                        .map(|b| format!("{b}█"))
                        .unwrap_or_else(|| tilde(&app.config.music_dir)),
                    true,
                ),
                Rescan => (
                    "Rescan library",
                    match app.scanning {
                        Some((done, total)) if total > 0 => format!("scanning… {done}/{total}"),
                        Some((done, _)) => format!("scanning… {done} files"),
                        None => format!("{} tracks indexed · Enter to rescan", app.library.tracks.len()),
                    },
                    false,
                ),
                LocalArtDir => ("Album-art cache", format!("{} · {}", tilde(&crate::config::art_cache_dir()), size(art.bytes)), false),
                Gapless => {
                    let g = app.gapless_secs();
                    let v = if g <= 0.0 { "off".to_string() } else { format!("{g:.1}s crossfade") };
                    ("Gapless playback", format!("◂ {v} ▸"), true)
                }
                KeepGoing => ("Keep playing similar songs", on_off(app.config.playback.keep_going).into(), true),
                Thumbnails => ("Cover thumbnails", on_off(app.show_album_thumbnails).into(), true),
                CacheAllArt => ("Pre-cache all album art", on_off(app.cache_all_art).into(), true),
                SortArticles => ("Sort \"The\" artists", on_off(app.config.ui.sort_articles).into(), true),
                BigIcons => ("Big transport icons", on_off(app.big_icons).into(), true),
                TabSpotify => ("Spotify tab", on_off(app.config.spotify.enabled).into(), true),
                TabExplore => ("Explore tab", on_off(app.config.browser.enabled).into(), true),
                TabPlaylists => ("Local Playlists tab", on_off(app.config.ui.show_playlists_tab).into(), true),
                SharedSearch => ("Shared search across tabs", on_off(app.config.ui.shared_search).into(), true),
                SpotifyCsv => (
                    "Export file / folder",
                    editing(PathField::SpotifyCsv)
                        .map(|b| format!("{b}█"))
                        .unwrap_or_else(|| tilde(&app.config.spotify.csv)),
                    true,
                ),
                SpotifyReimport => ("Re-import export", "Enter to merge in new tracks".into(), false),
                SpotifyLinks => (
                    "Linked playlists",
                    format!("{} linked · Enter to add a link", app.config.spotify.links.len()),
                    false,
                ),
                DownloadsDir => (
                    "Downloaded songs",
                    format!("{} · {} songs · {}", tilde(&crate::spotify::music_root()), audio.files, size(audio.bytes)),
                    false,
                ),
                SpotifyCoversDir => (
                    "Spotify/Explore covers",
                    format!(
                        "{} · {}",
                        tilde(&crate::config::spotify_cache_dir().join("covers")),
                        size(sp_covers.bytes)
                    ),
                    false,
                ),
                LibraryFile => ("Library file", tilde(&crate::config::streaming_store_path()), false),
                ConfigFile => ("Config file", tilde(&crate::config::config_path()), false),
            };
            let value_style = if strong { Style::default().fg(t.accent) } else { Style::default().fg(t.muted) };
            ListItem::new(Line::from(vec![
                Span::raw(format!("   {label:<label_w$}")),
                Span::styled(value, value_style),
            ]))
        })
        .collect();
    let title = " Settings ".to_string();
    let focused = app.panes_focused();
    render_list(f, &mut app.settings_state, items, area, &title, focused, &mut app.rects.settings, t, 1);
}

fn draw_playlists(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let cols =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).split(area);

    let list_focused = app.panes_focused() && app.pl_focus == PlPane::List;
    let tracks_focused = app.panes_focused() && app.pl_focus != PlPane::List;

    let names: Vec<ListItem> = if app.playlists.is_empty() {
        vec![ListItem::new(Span::raw("(no playlists — press n)").fg(t.dim))]
    } else {
        app.playlists
            .iter()
            .map(|p| ListItem::new(format!("{}  ({})", p.name, p.tracks.len())))
            .collect()
    };
    render_list(
        f,
        &mut app.playlist_state,
        names,
        cols[0],
        " Playlists ",
        list_focused,
        &mut app.rects.pl_list,
        t,
        1,
    );

    let pl_tracks: Vec<crate::model::Track> = app
        .playlist_state
        .selected()
        .and_then(|i| app.playlists.get(i))
        .map(|p| p.tracks.clone())
        .unwrap_or_default();
    let titems: Vec<ListItem> = pl_tracks
        .iter()
        .map(|tr| {
            ListItem::new(Line::from(vec![
                Span::raw(fit(&tr.title, 34)),
                Span::styled(
                    format!(" {:>5} ", fmt_time(tr.length_secs)),
                    Style::default().fg(t.muted),
                ),
                Span::styled(tr.track_artist.clone(), Style::default().fg(t.muted)),
            ]))
        })
        .collect();
    render_list(
        f,
        &mut app.pl_track_state,
        titems,
        cols[1],
        " Tracks ",
        tracks_focused,
        &mut app.rects.pl_tracks,
        t,
        1,
    );

    // Naming prompt for a new playlist.
    if let Some(name) = &app.editing_playlist_name {
        let w = 44.min(area.width.saturating_sub(4));
        let popup = Rect::new(area.x + (area.width - w) / 2, area.y + area.height / 2, w, 3);
        f.render_widget(Clear, popup);
        let line = Line::from(vec![
            Span::raw("Name: "),
            Span::styled(name.clone(), Style::default().fg(t.accent)),
            Span::styled("█", Style::default().fg(t.accent)),
        ]);
        let block = Block::bordered().border_type(BorderType::Rounded)
            .title(" New playlist (Enter / Esc) ")
            .border_style(Style::default().fg(t.accent));
        f.render_widget(Paragraph::new(line).block(block), popup);
    }
}

fn draw_spotify(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let pct = app.sp_pct;
    let cols = Layout::horizontal([Constraint::Percentage(pct), Constraint::Percentage(100 - pct)])
        .split(area);
    app.rects.sp_body = area;
    app.rects.sp_div = cols[1].x;
    let (songs, queue) = split_queue(app, cols[1]);
    let sp_list_focused = app.panes_focused() && app.sp_focus == PlPane::List;
    let sp_tracks_focused = app.panes_focused() && app.sp_focus == PlPane::Tracks;
    let thumbs = app.show_album_thumbnails;
    let row_h = app.sp_row_h();
    let indent = if thumbs { " ".repeat(ALBUM_THUMB_COLS as usize + 1) } else { String::new() };
    // Two-line rows (with thumbnails) put secondary info on the second line,
    // like Spotify's own track list.
    let row = |top: Line<'static>, sub: Line<'static>| -> ListItem<'static> {
        if thumbs {
            let mut a = vec![Span::raw(indent.clone())];
            a.extend(top.spans);
            let mut b = vec![Span::raw(indent.clone())];
            b.extend(sub.spans);
            ListItem::new(Text::from(vec![Line::from(a), Line::from(b)]))
        } else {
            ListItem::new(top)
        }
    };

    let list_w = cols[0].width.saturating_sub(3) as usize;
    let names: Vec<ListItem> = if app.sp_view.is_empty() && !app.spotify_lists.is_empty() {
        vec![ListItem::new(Span::raw("(no matches)").fg(t.muted))]
    } else if app.sp_view.is_empty() {
        let why = app.spotify_error.clone().unwrap_or_else(|| "export is empty".into());
        vec![
            ListItem::new(Span::raw("(nothing imported)").fg(t.muted)),
            ListItem::new(Span::raw(why).fg(t.muted)),
            ListItem::new(Span::raw("set the path in Settings (p)").fg(t.muted)),
        ]
    } else {
        app.sp_view
            .iter()
            .map(|p| {
                let n = p.tracks.len();
                if thumbs {
                    let w = list_w.saturating_sub(indent.len());
                    row(
                        Line::raw(fit(&p.name, w)),
                        Line::from(format!("{n} songs")).fg(t.muted),
                    )
                } else {
                    ListItem::new(format!("{}  ({n})", p.name))
                }
            })
            .collect()
    };
    let list_title = format!(" Library · {} ", app.sp_sort.label());
    render_list(
        f,
        &mut app.sp_list_state,
        names,
        cols[0],
        &list_title,
        sp_list_focused,
        &mut app.rects.sp_list,
        t,
        row_h,
    );

    let tracks: Vec<crate::model::Track> = app
        .selected_sp_list()
        .map(|p| p.tracks.clone())
        .unwrap_or_default();
    let playing = app.audio.current_path().map(|p| p.to_path_buf());
    // Only stat the cache for rows that can be on screen.
    let first = app.sp_track_state.offset();
    let shown = first..first + (songs.height / row_h) as usize;
    // Inner width minus the scrollbar column, the thumbnail indent, the
    // 2-cell state marker and the right-pinned length field.
    let text_w = (songs.width.saturating_sub(3) as usize).saturating_sub(indent.len() + 2);
    let titems: Vec<ListItem> = tracks
        .iter()
        .enumerate()
        .map(|(i, tr)| {
            let (mark, mark_style) = if playing.as_deref() == Some(tr.path.as_path()) {
                ("▶ ", Style::default().fg(t.accent))
            } else if app.fetching.contains(&tr.path) {
                ("⇣ ", Style::default().fg(t.accent))
            } else if shown.contains(&i) && tr.path.exists() {
                ("● ", Style::default().fg(t.muted))
            } else {
                ("  ", Style::default())
            };
            let len = if tr.length_secs > 0 { fmt_time(tr.length_secs) } else { "--:--".into() };
            let body_w = text_w.saturating_sub(len.chars().count() + 2);
            if thumbs {
                let sub = format!("{} · {}", tr.track_artist, tr.album);
                let heart = app.liked.contains(&tr.path);
                row(
                    Line::from(vec![
                        Span::styled(mark, mark_style),
                        Span::raw(fit(&tr.title, body_w.saturating_sub(2))),
                        Span::styled(if heart { " ♥" } else { "  " }, Style::default().fg(t.accent)),
                        Span::styled(format!(" {len} "), Style::default().fg(t.muted)),
                    ]),
                    Line::from(vec![
                        Span::raw("  "),
                        Span::styled(fit(&sub, body_w), Style::default().fg(t.muted)),
                    ]),
                )
            } else {
                let title_w = (body_w * 4 / 10).max(10);
                let artist_w = (body_w * 3 / 10).max(8);
                let album_w = body_w.saturating_sub(title_w + artist_w + 1);
                ListItem::new(Line::from(vec![
                    Span::styled(mark, mark_style),
                    Span::raw(fit(&tr.title, title_w)),
                    Span::styled(format!(" {len:>5} "), Style::default().fg(t.muted)),
                    Span::styled(fit(&tr.track_artist, artist_w), Style::default().fg(t.muted)),
                    Span::styled(format!(" {}", fit(&tr.album, album_w)), Style::default().fg(t.muted)),
                ]))
            }
        })
        .collect();
    render_list(
        f,
        &mut app.sp_track_state,
        titems,
        songs,
        " Songs ",
        sp_tracks_focused,
        &mut app.rects.sp_tracks,
        t,
        row_h,
    );

    if thumbs {
        let list_keys: Vec<_> = app.sp_view.iter().map(App::sp_list_thumb_key).collect();
        let track_keys: Vec<_> = tracks.iter().map(App::sp_track_thumb_key).collect();
        let (lr, lo) = (app.rects.sp_list, app.sp_list_state.offset());
        let (tr, to) = (app.rects.sp_tracks, app.sp_track_state.offset());
        draw_row_thumbs(f, lr, lo, &list_keys, &mut app.row_thumb_cache);
        draw_row_thumbs(f, tr, to, &track_keys, &mut app.row_thumb_cache);
    }
    if let Some(q) = queue {
        let focused = app.panes_focused() && app.sp_focus == PlPane::Queue;
        draw_queue(f, app, q, focused);
    }
}

fn draw_browser(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::deezer::Item;
    let t = app.theme;
    let pct = app.sp_pct;
    let cols = Layout::horizontal([Constraint::Percentage(pct), Constraint::Percentage(100 - pct)])
        .split(area);
    app.rects.sp_body = area;
    app.rects.sp_div = cols[1].x;
    let (songs, queue) = split_queue(app, cols[1]);
    let br_list_focused = app.panes_focused() && app.br_focus == PlPane::List;
    let br_tracks_focused = app.panes_focused() && app.br_focus == PlPane::Tracks;
    let thumbs = app.show_album_thumbnails;
    let row_h = app.sp_row_h();
    let indent = if thumbs { " ".repeat(ALBUM_THUMB_COLS as usize + 1) } else { String::new() };

    let n = app.br_section_count();
    let mut sections: Vec<ListItem> = (0..n)
        .filter_map(|i| app.br_section(i).map(|v| (i, v)))
        .map(|(i, v)| {
            let r = app.br_results.len();
            let building = app.br_radio_building.as_deref() == Some(v.title.as_str());
            let label = if building { format!("{}  (building…)", v.title) } else { format!("{}  ({})", v.title, v.items.len()) };
            // Search results and radios sit above the home sections; mark them apart.
            if i < r {
                ListItem::new(Line::from(vec![Span::styled("⌕ ", Style::default().fg(t.accent)), Span::raw(label)]))
            } else if i < r + app.br_radios.len() {
                ListItem::new(Line::from(vec![Span::styled("◉ ", Style::default().fg(t.accent)), Span::raw(label)]))
            } else {
                ListItem::new(Line::from(vec![Span::raw("  "), Span::raw(label)]))
            }
        })
        .collect();
    if sections.is_empty() {
        sections.push(ListItem::new(Span::raw("loading…").fg(t.muted)));
    }
    // With an artist/album page open on the right, no sidebar entry is
    // what's shown, so none is highlighted.
    let mut sec_state = app.br_sec_state.clone();
    if !app.br_stack.is_empty() {
        sec_state.select(None);
    }
    render_list(
        f,
        &mut sec_state,
        sections,
        cols[0],
        " Explore ",
        br_list_focused,
        &mut app.rects.br_sections,
        t,
        1,
    );
    if app.br_stack.is_empty() {
        app.br_sec_state = sec_state;
    }

    let view = app.br_current().cloned().unwrap_or_default();
    let playing = app.audio.current_path().map(|p| p.to_path_buf());
    let text_w = (songs.width.saturating_sub(3) as usize).saturating_sub(indent.len() + 2);
    let row = |top: Vec<Span<'static>>, sub: String| -> ListItem<'static> {
        if thumbs {
            let mut a = vec![Span::raw(indent.clone())];
            a.extend(top);
            ListItem::new(Text::from(vec![
                Line::from(a),
                Line::from(vec![
                    Span::raw(format!("{indent}  ")),
                    Span::styled(fit(&sub, text_w), Style::default().fg(t.muted)),
                ]),
            ]))
        } else {
            let mut a = top;
            a.push(Span::styled(format!("  {sub}"), Style::default().fg(t.muted)));
            ListItem::new(Line::from(a))
        }
    };
    let items: Vec<ListItem> = view
        .items
        .iter()
        .map(|item| match item {
            Item::Track(tr) => {
                let (mark, mark_style) = if playing.as_deref() == Some(tr.path.as_path()) {
                    ("▶ ", Style::default().fg(t.accent))
                } else if app.fetching.contains(&tr.path) {
                    ("⇣ ", Style::default().fg(t.accent))
                } else {
                    ("  ", Style::default())
                };
                let len = if tr.length_secs > 0 { fmt_time(tr.length_secs) } else { "--:--".into() };
                let heart = app.liked.contains(&tr.path);
                let title_w = text_w.saturating_sub(len.chars().count() + 4);
                let sub = if tr.album.is_empty() {
                    tr.track_artist.clone()
                } else {
                    format!("{} · {}", tr.track_artist, tr.album)
                };
                row(
                    vec![
                        Span::styled(mark, mark_style),
                        Span::raw(fit(&tr.title, title_w)),
                        Span::styled(if heart { " ♥" } else { "  " }, Style::default().fg(t.accent)),
                        Span::styled(format!(" {len} "), Style::default().fg(t.muted)),
                    ],
                    sub,
                )
            }
            Item::Album { title, artist, .. } => row(
                vec![Span::raw("◉ "), Span::raw(fit(title, text_w))],
                format!("Album · {artist}"),
            ),
            Item::Artist { name, .. } => {
                row(vec![Span::raw("☺ "), Span::raw(fit(name, text_w))], "Artist".into())
            }
            Item::Playlist { title, count, .. } => row(
                vec![Span::raw("≡ "), Span::raw(fit(title, text_w))],
                format!("Playlist · {count} songs"),
            ),
        })
        .collect();
    let mut title = String::from(" ");
    for v in &app.br_stack {
        title.push_str(&v.title);
        title.push_str(" › ");
    }
    if app.br_stack.is_empty() {
        title.push_str(&view.title);
    } else {
        title.truncate(title.len() - " › ".len());
    }
    if let Some(what) = &app.br_loading {
        title.push_str(&format!("  (loading {what})"));
    }
    if app.br_stack.is_empty() && app.br_radio_building.as_deref() == Some(view.title.as_str()) {
        title.push_str("  (building — about 20 s)");
    }
    title.push(' ');
    render_list(
        f,
        &mut app.br_item_state,
        items,
        songs,
        &title,
        br_tracks_focused,
        &mut app.rects.br_items,
        t,
        row_h,
    );

    if thumbs {
        let mut keys = Vec::with_capacity(view.items.len());
        for i in &view.items {
            keys.push(App::br_item_thumb(i).0);
        }
        let (r, o) = (app.rects.br_items, app.br_item_state.offset());
        draw_row_thumbs(f, r, o, &keys, &mut app.row_thumb_cache);
    }
    if let Some(q) = queue {
        let focused = app.panes_focused() && app.br_focus == PlPane::Queue;
        draw_queue(f, app, q, focused);
    }
}

/// The `m` "add to playlist" popup, centered over the body

fn draw_add_menu(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let targets = app.add_targets();
    let Some(menu) = &app.add_menu else { return };
    let what = match menu.tracks.as_slice() {
        [one] => format!("'{}'", one.title),
        many => format!("{} tracks", many.len()),
    };
    let w = 56.min(area.width.saturating_sub(4));
    let h = (targets.len() as u16 + 2).min(area.height.saturating_sub(2)).max(3);
    let popup = Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, popup);
    if let Some(name) = &menu.naming {
        let popup = Rect::new(popup.x, popup.y, w, 3);
        f.render_widget(Clear, popup);
        let line = Line::from(vec![
            Span::raw("Name: "),
            Span::styled(name.clone(), Style::default().fg(t.accent)),
            Span::styled("█", Style::default().fg(t.accent)),
        ]);
        let block = Block::bordered().border_type(BorderType::Rounded)
            .title(" New playlist (Enter / Esc) ")
            .border_style(Style::default().fg(t.accent));
        f.render_widget(Paragraph::new(line).block(block), popup);
        return;
    }
    let items: Vec<ListItem> = targets.iter().map(|(label, _)| ListItem::new(label.clone())).collect();
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(menu.sel));
    let list = List::new(items)
        .block(
            Block::bordered().border_type(BorderType::Rounded)
                .title(format!(" Add {what} to… (Enter / Esc) "))
                .border_style(Style::default().fg(t.accent)),
        )
        .highlight_style(Style::default().bg(t.accent).fg(t.selection_fg));
    f.render_stateful_widget(list, popup, &mut state);
}

fn draw_seekbar(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let dur = app.audio.duration().as_secs();
    let ratio = if let Some(frac) = app.pending_seek {
        frac
    } else if dur > 0 {
        (app.audio.position().as_secs() as f64 / dur as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let pos = (ratio * dur as f64) as u64;
    let label = format!("{} / {}", fmt_time(pos), fmt_time(dur));
    let block = titled(" Progress ", app.focus == Focus::Seekbar, t);
    let inner = block.inner(area);
    app.rects.seekbar = inner;

    // Full-width gauge with no built-in label (it would centre over the whole
    // bar, not the transport). The label is drawn by hand below.
    let gauge = Gauge::default()
        .block(block)
        .gauge_style(Style::default().fg(t.accent).bg(Color::Reset))
        .ratio(ratio)
        .label("");
    f.render_widget(gauge, area);

    if inner.height > 0 && inner.width > 0 {
        // Same horizontal split as draw_bottom so the label centres over the buttons.
        let parts = Layout::horizontal([
            Constraint::Length(16),
            Constraint::Min(10),
            Constraint::Length(5),
        ])
        .split(area);
        // parts[1].width is odd at common terminal widths (even − 21), so centring
        // floors the padding and lands "/" on the right cell of the 2-wide play
        // glyph; expanding 1 cell left shifts it onto the left (dominant) cell.
        let lx = parts[1].x.saturating_sub(1);
        let lw = parts[1].width + 1;
        let chars: Vec<char> = label.chars().collect();
        let start = lx + lw.saturating_sub(chars.len() as u16) / 2;
        // Column where the gauge fill ends; the label inverts across it, exactly
        // like a built-in gauge label (dark-on-accent over fill, accent-on-bg off).
        let fill_end = inner.x + (inner.width as f64 * ratio).round() as u16;
        let right_edge = inner.x + inner.width;
        let buf = f.buffer_mut();
        for (i, ch) in chars.iter().enumerate() {
            let x = start + i as u16;
            if x >= right_edge {
                break;
            }
            let style = if x < fill_end {
                Style::default().fg(t.selection_fg).bg(t.accent)
            } else {
                Style::default().fg(t.accent).bg(Color::Reset)
            };
            if let Some(cell) = buf.cell_mut((x, inner.y)) {
                cell.set_symbol(&ch.to_string());
                cell.set_style(style);
            }
        }
    }
}

fn draw_bottom(f: &mut Frame, app: &mut App, area: Rect) {
    let parts = Layout::horizontal([
        Constraint::Length(16), // album art
        Constraint::Min(10),    // transport (centered)
        Constraint::Length(5),  // volume slider, far right
    ])
    .split(area);
    draw_art(f, app, parts[0]);
    draw_transport(f, app, parts[1]);
    draw_volume(f, app, parts[2]);
}

fn draw_volume(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let vol = (app.audio.volume() * 100.0).round() as u64;
    let block = titled("Vol", app.focus == Focus::Volume, t);
    let inner = block.inner(area);
    app.rects.volume = inner;
    let bar = Bar::default()
        .value(vol)
        .text_value(format!("{vol}"))
        .style(Style::default().fg(t.accent))
        .value_style(Style::default().fg(t.selection_fg).bg(t.accent));
    let chart = BarChart::default()
        .block(block)
        .data(BarGroup::default().bars(&[bar]))
        .max(100)
        .bar_width(inner.width.max(1))
        .bar_gap(0);
    f.render_widget(chart, area);
}

fn draw_art(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    app.rects.art = area;
    if let Some(proto) = app.art_proto.as_mut() {
        let img = StatefulImage::default().resize(Resize::Fit(None));
        f.render_stateful_widget(img, area, proto);
    } else {
        let placeholder = Paragraph::new("\n♪").alignment(Alignment::Center).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(t.border)),
        );
        f.render_widget(placeholder, area);
    }
}

fn draw_transport(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::model::RepeatMode;
    let t = app.theme;
    let playing = app.audio.is_playing();
    // Material Design media glyphs — one uniform family (unlike Font Awesome,
    // whose shuffle/repeat come from unrelated icon families and render at
    // different sizes). The repeat button uses the off/all/one variants.
    let play_icon = if playing { "\u{f03e4}" } else { "\u{f040a}" }; // pause / play
    let repeat_glyph = match app.repeat {
        RepeatMode::Off => "\u{f0457}", // md-repeat-off
        RepeatMode::All => "\u{f0456}", // md-repeat
        RepeatMode::One => "\u{f0458}", // md-repeat-once
    };
    let icons: [(&str, bool); 5] = [
        ("\u{f049d}", app.shuffle),                   // md-shuffle
        ("\u{f04ae}", false),                         // md-skip-previous
        (play_icon, playing),                         // play / pause
        ("\u{f04ad}", false),                         // md-skip-next
        (repeat_glyph, app.repeat != RepeatMode::Off), // md-repeat*
    ];

    // Colour reflects on/off state only (active → accent, else muted); the
    // keyboard cursor is shown with an underline, so an "off" button sitting
    // under the cursor doesn't look enabled.
    let sel = app.transport_sel;
    let row_focused = app.focus == Focus::Transport;
    let style_for = |i: usize, active: bool| -> Style {
        let s = Style::default().fg(if active { t.accent } else { t.muted });
        if row_focused && i == sel {
            s.add_modifier(Modifier::UNDERLINED)
        } else {
            s
        }
    };

    let rects = [
        &mut app.rects.btn_shuffle,
        &mut app.rects.btn_prev,
        &mut app.rects.btn_play,
        &mut app.rects.btn_next,
        &mut app.rects.btn_repeat,
    ];

    if app.big_icons {
        // 2x glyphs via the kitty text-sizing protocol; each occupies 2x2 cells.
        let (bw, gap) = (2u16, 5u16);
        let total = 5 * bw + 4 * gap;
        // Centre the 2-row icon block in the space above the status line.
        // area.height=7, status takes last row → 6 usable rows; (6-2)/2 = 2.
        let by = area.y + area.height.saturating_sub(3) / 2;
        let mut x = area.x + area.width.saturating_sub(total) / 2;
        let buf = f.buffer_mut();
        for (i, (glyph, active)) in icons.iter().enumerate() {
            *rects[i] = Rect::new(x, by, bw, 2);
            // These MD glyphs differ in height (skip ≈500u, play ≈582u, shuffle
            // ≈668u, repeat ≈832u of 1000 em) but share a vertical centre (~360u),
            // so centre-align (v=2) to line up their centres. Top/bottom aligning
            // would splay glyphs of different heights apart. The size differences
            // themselves are intrinsic to the font and can't be normalized here.
            draw_big_glyph(buf, x, by, glyph, style_for(i, *active), 2);
            x += bw + gap;
        }
    } else {
        let gap = 2u16;
        let labels: [String; 5] = std::array::from_fn(|i| format!("  {}  ", icons[i].0));
        let widths: Vec<u16> = labels.iter().map(|l| l.chars().count() as u16).collect();
        let total: u16 = widths.iter().sum::<u16>() + gap * 4;
        let y = area.y + area.height / 2;
        let mut x = area.x + area.width.saturating_sub(total) / 2;
        for (i, (_, active)) in icons.iter().enumerate() {
            *rects[i] = Rect::new(x, y, widths[i], 1);
            f.render_widget(
                Paragraph::new(Span::styled(labels[i].clone(), style_for(i, *active))),
                Rect::new(x, y, widths[i], 1),
            );
            x += widths[i] + gap;
        }
    }

    // Status line beneath the buttons.
    if area.height >= 2 {
        let info = Span::styled(app.status.clone(), Style::default().fg(t.muted));
        f.render_widget(
            Paragraph::new(info).alignment(Alignment::Center),
            Rect::new(area.x, area.y + area.height.saturating_sub(1), area.width, 1),
        );
    }
}

/// Split items with these heights, in order, into at most `n` columns so
/// the tallest column is as short as possible (each break also saves the
/// blank line between sections).
fn balanced_columns(heights: &[usize], n: usize) -> Vec<Vec<usize>> {
    fn best(h: &[usize], from: usize, n: usize) -> (usize, Vec<usize>) {
        let rest: usize = h[from..].iter().sum();
        if n == 1 || from + 1 >= h.len() {
            return (rest, vec![h.len()]);
        }
        let mut top = (usize::MAX, Vec::new());
        let mut col = 0;
        for end in from + 1..h.len() {
            col += h[end - 1];
            let (tail, mut cuts) = best(h, end, n - 1);
            let tallest = col.max(tail);
            if tallest < top.0 {
                cuts.insert(0, end);
                top = (tallest, cuts);
            }
        }
        if rest < top.0 { (rest, vec![h.len()]) } else { top }
    }
    let mut start = 0;
    best(heights, 0, n.max(1))
        .1
        .into_iter()
        .map(|end| {
            let col = (start..end).collect();
            start = end;
            col
        })
        .filter(|c: &Vec<usize>| !c.is_empty())
        .collect()
}

/// The `?` help panel: every shortcut, grouped by task, in balanced columns.
/// The current tab's section comes first.
fn draw_help(f: &mut Frame, area: Rect, tab: Tab, t: Theme) {
    type Section = (&'static str, Vec<(&'static str, &'static str)>);
    let songs: Section = (
        "Spotify & Explore",
        vec![
            ("g", "go to: radio · artist · album"),
            ("G", "keep going after the queue"),
            ("f", "like / unlike"),
            ("m", "add to a playlist"),
            ("x", "wrong song? re-match"),
            ("S", "save list (Explore)"),
            ("⌫", "back (Explore)"),
            ("o", "sort (Spotify)"),
            ("u  d", "link / unlink (Spotify)"),
            ("R", "re-import (Spotify)"),
        ],
    );
    let local_playlists: Section = (
        "Local playlists",
        vec![
            ("n", "new from queue"),
            ("a", "append the queue"),
            ("d", "delete playlist"),
            ("x", "remove song"),
        ],
    );
    let mut sections: Vec<Section> = vec![
        (
            "Getting around",
            vec![
                ("1 – 5", "switch tab"),
                ("Tab  ⇧Tab", "next / previous area"),
                ("↑ ↓  j k", "move"),
                ("← →  h l", "change pane"),
                ("Enter", "play / open"),
                ("/", "search"),
                ("Esc", "clear search · back"),
                ("?", "this help"),
                ("q", "quit"),
            ],
        ),
        (
            "Playback",
            vec![
                ("Space", "play / pause"),
                ("n  p", "next / previous"),
                ("[  ]", "seek −5 s / +5 s"),
                ("+  −", "volume"),
                ("s", "shuffle"),
                ("r", "repeat: off · all · one"),
                ("i", "big album art"),
            ],
        ),
        (
            "Queue",
            vec![
                ("a", "add to queue"),
                ("Q", "show / hide queue"),
                ("d", "remove (in the queue)"),
                ("J  K", "move up / down"),
                ("c", "clear queue"),
            ],
        ),
        songs,
        local_playlists,
        (
            "Mouse & layout",
            vec![
                ("click", "select · run a hint"),
                ("double-click", "play"),
                ("drag", "seek · volume · scroll"),
                ("drag divider", "resize panes"),
                ("Alt+[  Alt+]", "resize focused pane"),
            ],
        ),
        (
            "Search editing",
            vec![
                ("^A  ^E", "start / end of line"),
                ("^W  Alt+⌫", "delete word"),
                ("^U  ^K", "delete to start / end"),
                ("^Y", "paste deleted text"),
                ("Alt+B  Alt+F", "word left / right"),
            ],
        ),
    ];
    // The section for the tab in use goes first.
    let mine = match tab {
        Tab::Streaming | Tab::Browser => Some(3),
        Tab::Playlists => Some(4),
        _ => None,
    };
    if let Some(i) = mine {
        let s = sections.remove(i);
        sections.insert(0, s);
    }

    let w = area.width.saturating_sub(4).min(120);
    let ncols: usize = if w >= 105 { 3 } else if w >= 68 { 2 } else { 1 };
    // Split the sections, in order, into columns with the shortest tallest
    // column (a blank line separates sections).
    let heights: Vec<usize> = sections.iter().map(|s| s.1.len() + 2).collect();
    let cols = balanced_columns(&heights, ncols);
    let col_w = (w.saturating_sub(2) / ncols as u16) as usize;
    let col_lines: Vec<Vec<Line>> = cols
        .iter()
        .map(|idxs| {
            let mut lines = Vec::new();
            for (n, &i) in idxs.iter().enumerate() {
                if n > 0 {
                    lines.push(Line::raw(""));
                }
                let (name, entries) = &sections[i];
                // The current tab's section header is filled in.
                let here = mine.is_some() && i == 0;
                let label = format!(" {name} ");
                let label_style = if here {
                    Style::default().fg(t.selection_fg).bg(t.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                };
                let rule = col_w.saturating_sub(label.chars().count() + 3);
                lines.push(Line::from(vec![
                    Span::styled("─", Style::default().fg(t.border)),
                    Span::styled(label, label_style),
                    Span::styled("─".repeat(rule), Style::default().fg(t.border)),
                ]));
                let key_w = entries.iter().map(|e| e.0.chars().count()).max().unwrap_or(1);
                for (key, desc) in entries {
                    lines.push(Line::from(vec![
                        Span::styled(format!(" {key:>key_w$}  "), Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
                        Span::raw(fit(desc, col_w.saturating_sub(key_w + 4))),
                    ]));
                }
            }
            lines
        })
        .collect();

    let rows = col_lines.iter().map(Vec::len).max().unwrap_or(0) as u16;
    let h = (rows + 4).min(area.height);
    let popup = Rect::new(area.x + area.width.saturating_sub(w) / 2, area.y + area.height.saturating_sub(h) / 2, w, h);
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(Span::styled(" Keyboard shortcuts ", Style::default().fg(t.accent).add_modifier(Modifier::BOLD))))
        .title_bottom(Line::from(Span::styled(" any key closes ", Style::default().fg(t.muted))).right_aligned())
        .border_style(Style::default().fg(t.accent))
        .padding(ratatui::widgets::Padding::new(1, 1, 1, 0));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let parts = Layout::horizontal(vec![Constraint::Ratio(1, ncols as u32); ncols]).split(inner);
    for (lines, rect) in col_lines.into_iter().zip(parts.iter()) {
        f.render_widget(Paragraph::new(lines), *rect);
    }
}

// --- helpers -------------------------------------------------------------

/// Draw a glyph at 2x using the kitty text-sizing protocol (OSC 66). The glyph
/// occupies a 2x2 cell block: the origin cell carries the escape, and the three
/// covered cells are marked skip so the diff won't overwrite them.
fn draw_big_glyph(buf: &mut Buffer, x: u16, y: u16, glyph: &str, style: Style, valign: u8) {
    // s=2:w=1 forces exactly 2 columns × 2 rows regardless of the glyph's
    // reported character width; h=2 centers horizontally, valign sets the
    // vertical placement (0=top, 1=bottom, 2=center).
    if let Some(c) = buf.cell_mut((x, y)) {
        c.set_symbol(&format!("\u{1b}]66;s=2:w=1:v={valign}:h=2;{glyph}\u{7}"));
        c.set_style(style);
    }
    for (dx, dy) in [(1u16, 0u16), (0, 1), (1, 1)] {
        if let Some(c) = buf.cell_mut((x + dx, y + dy)) {
            c.set_skip(true);
        }
    }
}

fn border_style(focused: bool, t: Theme) -> Style {
    if focused {
        Style::default().fg(t.accent)
    } else {
        Style::default().fg(t.border)
    }
}

/// A bordered block whose title is accent + bold when focused, and the muted
/// gray (matching the now-playing status line) when not.
fn titled(title: &str, focused: bool, t: Theme) -> Block<'_> {
    let title_style = if focused {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.muted)
    };
    Block::bordered().border_type(BorderType::Rounded)
        .title(Span::styled(title, title_style))
        .border_style(border_style(focused, t))
}

#[allow(clippy::too_many_arguments)]
fn render_list(
    f: &mut Frame,
    state: &mut ratatui::widgets::ListState,
    items: Vec<ListItem>,
    area: Rect,
    title: &str,
    focused: bool,
    rect_out: &mut Rect,
    t: Theme,
    row_h: u16,
) {
    let block = titled(title, focused, t);
    let inner = block.inner(area);
    *rect_out = inner;
    f.render_widget(block, area);
    // Both states set an explicit bg + fg so the whole selected row reads as one
    // uniform bar; relying on REVERSED instead would swap each span's own color
    // into the background, tinting differently-colored columns unevenly. No
    // BOLD: terminals that render bold as bright turn the default black
    // selection_fg into bright black (gray), unreadable on most accents.
    let highlight = if focused {
        Style::default().bg(t.accent).fg(t.selection_fg)
    } else {
        Style::default().bg(t.muted).fg(t.selection_fg)
    };
    let len = items.len();
    let list = List::new(items).highlight_style(highlight).highlight_symbol("");
    // Stop the highlighted row 1 cell short of the border: List fills the
    // whole area it's given with the highlight bg, so rendering into the
    // full inner rect would run the bar flush into the scrollbar column and
    // the two would fuse into one solid band when they're the same color.
    let content = Rect { width: inner.width.saturating_sub(1), ..inner };

    // Manage the scroll offset ourselves instead of handing `state` straight
    // to `List`: its built-in "keep the selection visible" auto-scroll is
    // exactly what fought a scrollbar-drag trying to pan the view
    // independent of the current selection (the view kept snapping back to
    // wherever `selected` was). `state`'s offset is the single source of
    // truth — kept in sync by `App::scroll_into_view` for keyboard nav and
    // `App::scroll_offset` for dragging — so render with a throwaway
    // `ListState` carrying that same offset, passing `selected` through only
    // when it's already within view (so the normal case still highlights the
    // active row exactly as before).
    let visible = (inner.height / row_h.max(1)) as usize;
    let max_offset = len.saturating_sub(visible.max(1));
    let offset = state.offset().min(max_offset);
    *state.offset_mut() = offset;
    let mut scratch = ratatui::widgets::ListState::default();
    *scratch.offset_mut() = offset;
    if state.selected().is_some_and(|s| s >= offset && s < offset + visible) {
        scratch.select(state.selected());
    }
    f.render_stateful_widget(list, content, &mut scratch);

    // Only show a scrollbar once content actually overflows the viewport.
    // Drawn by hand rather than with ratatui's `Scrollbar`: that rounds the
    // thumb's start and end independently, so its length wobbled by a cell
    // as the list scrolled. Here the length is fixed by the viewport/content
    // ratio and only the start moves, spanning the full track from
    // `offset == 0` (top) to `offset == max_offset` (flush with the bottom).
    // It paints `inner`'s last column (not the block's border): the right
    // border doubles as the column-resize grab zone (`near_divider` in
    // input.rs), and that column is the 1-cell gap left by the highlight fix
    // above, which nothing else claims.
    if len > visible && inner.width > 0 && inner.height > 0 {
        let track = inner.height as usize;
        let thumb_len = ((track * visible + len / 2) / len).clamp(1, track);
        let thumb_start = ((track - thumb_len) * offset + max_offset / 2) / max_offset.max(1);
        let x = inner.x + inner.width - 1;
        let style = Style::default().fg(if focused { t.accent } else { t.muted });
        let buf = f.buffer_mut();
        for row in thumb_start..thumb_start + thumb_len {
            buf[(x, inner.y + row as u16)].set_symbol("█").set_style(style);
        }
    }
}

/// Truncate `s` to `max` chars with ellipsis; pad with spaces if shorter (anchors next column).
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        format!("{s:<max$}")
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Strip a leading `[YYYY]` or `(YYYY)` prefix from an album title for display.
/// Handles DB rows that were scanned before the strip was applied at scan time.
fn strip_year_bracket(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 6 && (b[0] == b'[' || b[0] == b'(') {
        let close = if b[0] == b'[' { b']' } else { b')' };
        if b[5] == close && b[1..5].iter().all(|c| c.is_ascii_digit()) {
            return s[6..].trim_start();
        }
    }
    s
}
