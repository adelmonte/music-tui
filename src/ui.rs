use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;
use ratatui_image::{Resize, StatefulImage};

use crate::app::{App, Focus, PlPane, Tab};
use crate::config::Theme;
use crate::model::{display_artist, fmt_time};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // tab bar
        Constraint::Length(3), // search
        Constraint::Min(5),    // body
        Constraint::Length(3), // seek bar
        Constraint::Length(7), // art + transport
    ])
    .split(f.area());

    draw_tabs(f, app, chunks[0]);
    draw_search(f, app, chunks[1]);
    match app.tab {
        Tab::Browser => draw_body(f, app, chunks[2]),
        Tab::Playlists => draw_playlists(f, app, chunks[2]),
        Tab::Settings => draw_library(f, app, chunks[2]),
    }
    draw_seekbar(f, app, chunks[3]);
    draw_bottom(f, app, chunks[4]);

    if app.show_help {
        draw_help(f, f.area(), app.theme);
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
    let block = Block::bordered()
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
    let labels = [" (1) Browser ", " (2) Playlists ", " (3) Settings "];
    let active = [
        app.tab == Tab::Browser,
        app.tab == Tab::Playlists,
        app.tab == Tab::Settings,
    ];
    let mut rects = [Rect::default(); 3];
    let mut x = area.x;
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            f.render_widget(
                Paragraph::new(Span::styled("│", Style::default().fg(t.border))),
                Rect::new(x, area.y, 1, 1),
            );
            x += 1;
        }
        let w = label.chars().count() as u16;
        let r = Rect::new(x, area.y, w, 1);
        rects[i] = r;
        let style = if active[i] {
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.muted)
        };
        f.render_widget(Paragraph::new(Span::styled(*label, style)), r);
        x += w;
    }
    app.rects.tab_browser = rects[0];
    app.rects.tab_playlists = rects[1];
    app.rects.tab_settings = rects[2];

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

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let focused = app.focus == Focus::Search;
    let block = titled(" Search (title / artist / album) ", focused, t);
    let inner = block.inner(area);
    app.rects.search = inner;
    let mut spans = vec![Span::raw("🔎 ")];
    if app.search.is_empty() && !focused {
        spans.push(Span::raw("Click or press / to search").fg(t.dim));
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
    let qp = app.queue_pct;
    let right =
        Layout::vertical([Constraint::Percentage(100 - qp), Constraint::Percentage(qp)])
            .split(cols[2]);
    app.rects.col3 = cols[2];
    app.rects.queue_div = right[1].y;

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

    // Queue.
    let now = app.now;
    let items: Vec<ListItem> = app
        .queue
        .iter()
        .enumerate()
        .map(|(i, tr)| {
            let marker = if Some(i) == now { "▶ " } else { "  " };
            let style = if Some(i) == now {
                Style::default().fg(t.accent)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, style),
                Span::styled(format!("{} — {}", tr.track_artist, tr.title), style),
            ]))
        })
        .collect();
    render_list(
        f,
        &mut app.queue_state,
        items,
        right[1],
        " Queue ",
        app.focus == Focus::Queue,
        &mut app.rects.queue,
        t,
        1,
    );
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
    let row_h = App::ALBUM_ROW_HEIGHT;
    if area.height < row_h || area.width <= ALBUM_THUMB_COLS {
        return;
    }
    let visible = (area.height / row_h) as usize;
    let offset = app.album_state.offset();
    let end = (offset + visible).min(app.view_albums.len());
    for row_i in offset..end {
        let album = app.view_albums[row_i].clone();
        let Some(proto) = app.row_thumb_cache.get_mut(&album) else {
            continue;
        };
        let y = area.y + ((row_i - offset) as u16) * row_h;
        let rect = Rect::new(area.x, y, ALBUM_THUMB_COLS, row_h);
        let img = StatefulImage::default().resize(Resize::Fit(None));
        f.render_stateful_widget(img, rect, proto);
    }
}

fn draw_library(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let block = titled(" Settings ", false, t);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let dir_line = match &app.editing_path {
        Some(buf) => Line::from(vec![
            Span::raw("Music dir: "),
            Span::styled(buf.clone(), Style::default().fg(t.accent)),
            Span::styled("█", Style::default().fg(t.accent)),
        ]),
        None => Line::from(vec![
            Span::raw("Music dir: "),
            Span::styled(
                app.config.music_dir.to_string_lossy().into_owned(),
                Style::default().fg(t.accent),
            ),
        ]),
    };

    let status = match app.scanning {
        Some((done, total)) if total > 0 => format!("Scanning… {done}/{total}"),
        Some((done, _)) => format!("Scanning… {done} files"),
        None => format!("{} tracks indexed.", app.library.tracks.len()),
    };

    let on_off = |b: bool| if b { "on" } else { "off" };
    let gapless = app.gapless_secs();
    let gapless_str = if gapless <= 0.0 { "off".to_string() } else { format!("{gapless:.1}s crossfade") };

    let val = |label: &str, v: &str| -> Line<'static> {
        Line::from(vec![
            Span::raw(format!("  {label:<26}")),
            Span::styled(v.to_string(), Style::default().fg(t.accent)),
        ])
    };

    let text = vec![
        Line::raw(""),
        dir_line,
        Line::raw(""),
        Line::from(status).fg(t.muted),
        Line::raw(""),
        val("Gapless playback",        &gapless_str),
        val("Cache all album art",      on_off(app.cache_all_art)),
        val("Sort \"The\" artists",     on_off(app.config.ui.sort_articles)),
        val("Big transport icons",      on_off(app.big_icons)),
        val("Album thumbnails",         on_off(app.show_album_thumbnails)),
        Line::raw(""),
        Line::from("  e    edit music directory path").fg(t.muted),
        Line::from("  r    rescan library").fg(t.muted),
        Line::from("  +/-  gapless fade (0 = off)").fg(t.muted),
        Line::from("  c    cache all album art").fg(t.muted),
        Line::from("  t    sort \"The\" artists").fg(t.muted),
        Line::from("  k    big transport icons").fg(t.muted),
        Line::from("  v    album thumbnails").fg(t.muted),
        Line::raw(""),
        Line::from(format!("  config: {}", crate::config::config_path_display())).fg(t.muted),
    ];
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

fn draw_playlists(f: &mut Frame, app: &mut App, area: Rect) {
    let t = app.theme;
    let cols =
        Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)]).split(area);

    let list_focused = app.pl_focus == PlPane::List;
    let tracks_focused = app.pl_focus == PlPane::Tracks;

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
        " Tracks (Enter to play · n new from queue · a append · d delete · x remove) ",
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
        let block = Block::bordered()
            .title(" New playlist (Enter / Esc) ")
            .border_style(Style::default().fg(t.accent));
        f.render_widget(Paragraph::new(line).block(block), popup);
    }
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

fn draw_help(f: &mut Frame, area: Rect, t: Theme) {
    // Two columns so the full list fits even on short (80x24) terminals.
    let left = help_lines(
        &[
            (
                "Global",
                &[
                    ("?", "toggle help"),
                    ("i", "album art popup"),
                    ("q / ^C", "quit"),
                    ("Tab", "cycle focus"),
                    ("1/2/3", "browser/playlists/settings"),
                ],
            ),
            (
                "Search",
                &[
                    ("/", "focus search"),
                    ("type", "filter results"),
                    ("Enter/↓", "go to results"),
                    ("Esc", "clear search"),
                    ("^A / ^E", "start / end of line"),
                    ("^B/^F ←→", "move by char"),
                    ("Alt+B/F", "move by word"),
                    ("^W / Alt+⌫", "del word back"),
                    ("Alt+D", "del word forward"),
                    ("^U / ^K", "kill to start / end"),
                    ("^Y", "yank (paste kill)"),
                ],
            ),
            (
                "Browse",
                &[
                    ("↑↓ k j", "move selection"),
                    ("←→ h l", "change column"),
                    ("Enter", "open / play"),
                    ("a", "queue item"),
                    ("a (queue)", "remove item"),
                    ("c", "clear queue"),
                ],
            ),
        ],
        t,
    );
    let right = help_lines(
        &[
            (
                "Playback",
                &[
                    ("Space", "play / pause"),
                    ("n / p", "next / prev"),
                    ("s", "shuffle"),
                    ("r", "repeat off/all/one"),
                    ("+ / -", "volume"),
                    ("[ / ]", "seek −/+ 5s"),
                ],
            ),
            (
                "Playlists tab",
                &[
                    ("n", "new from queue"),
                    ("a", "append queue"),
                    ("d", "delete playlist"),
                    ("x", "remove track"),
                    ("Enter", "play"),
                ],
            ),
            (
                "Layout & mouse",
                &[
                    ("Alt+] [", "resize col / queue"),
                    ("drag div", "resize columns"),
                    ("drag bars", "scrub / set"),
                    ("click", "select anything"),
                ],
            ),
        ],
        t,
    );

    let rows = left.len().max(right.len()) as u16;
    let w = 60.min(area.width.saturating_sub(4));
    let h = (rows + 2).min(area.height.saturating_sub(2));
    let popup = Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Help ")
        .border_style(Style::default().fg(t.accent));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let halves = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    f.render_widget(Paragraph::new(left), halves[0]);
    f.render_widget(Paragraph::new(right), halves[1]);
}

/// Build the help lines for one column: underlined accent section headers, then
/// accent-bold keys aligned in a fixed column with muted descriptions.
fn help_lines(sections: &[(&str, &[(&str, &str)])], t: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, (name, entries)) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![
            Span::styled(" ", Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                name.to_string(),
                Style::default()
                    .fg(t.accent)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ),
        ]));
        for (key, desc) in entries.iter() {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {key:<9}"),
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled((*desc).to_string(), Style::default().fg(t.muted)),
            ]));
        }
    }
    lines
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
    Block::bordered()
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
    // into the background, tinting differently-colored columns unevenly.
    let highlight = if focused {
        Style::default()
            .bg(t.accent)
            .fg(t.selection_fg)
            .add_modifier(Modifier::BOLD)
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
    if len > visible {
        // `ScrollbarState`'s thumb math reserves a trailing `viewport_length`
        // worth of track for content_length (it assumes `position` can reach
        // `content_length - 1`, i.e. scrolling until only the last item is
        // pinned to the *top* of the view). Our `offset` only ever reaches
        // `max_offset` (the last item pinned to the *bottom*), so feeding it
        // raw `len` left the thumb short of the bottom edge — content_length
        // needs to be `max_offset + 1` for `position == max_offset` to map to
        // the very end of the track.
        let mut sb_state = ScrollbarState::new(max_offset + 1).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_style(Style::default().fg(if focused { t.accent } else { t.muted }));
        // Render into `inner` (not the block's own border column): the box's
        // right border doubles as the column-resize grab zone (`near_divider`
        // in input.rs grabs both the divider line and the column one to its
        // left), so painting the thumb there made dragging a column border
        // fight with the scrollbar sharing the same cell. `inner`'s own last
        // column is the 1-cell gap left by the highlight fix above, which
        // isn't claimed by anything else.
        f.render_stateful_widget(scrollbar, inner, &mut sb_state);
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
