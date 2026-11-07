//! The right-click menu: a list of commands, each showing its shortcut, with
//! one level of submenus (Add to playlist ▸, Sort ▸).

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{AddTarget, GotoKind, SpSort};
use crate::config::Theme;
use crate::keys::Action;

/// What a menu item does.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuCmd {
    Act(Action),
    /// Play the highlighted item (an artist or album plays all its songs).
    Play,
    AddTo(AddTarget),
    Goto(GotoKind),
    Sort(SpSort),
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    /// The shortcut, shown on the right.
    pub key: String,
    pub cmd: Option<MenuCmd>,
    pub submenu: Vec<MenuItem>,
    pub separator: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, key: String, cmd: MenuCmd) -> Self {
        Self { label: label.into(), key, cmd: Some(cmd), submenu: Vec::new(), separator: false }
    }

    pub fn sub(label: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self { label: label.into(), key: String::new(), cmd: None, submenu: items, separator: false }
    }

    pub fn sep() -> Self {
        Self { label: String::new(), key: String::new(), cmd: None, submenu: Vec::new(), separator: true }
    }

    fn selectable(&self) -> bool {
        !self.separator
    }
}

pub struct Menu {
    pub title: Option<String>,
    pub items: Vec<MenuItem>,
    pub pos: (u16, u16),
    pub sel: usize,
    /// Highlighted submenu item while its submenu is open.
    pub sub: Option<usize>,
    pub rect: Rect,
    pub sub_rect: Rect,
}

pub enum Outcome {
    /// Keep the menu open.
    Stay,
    Close,
    Run(MenuCmd),
}

impl Menu {
    pub fn new(title: Option<String>, mut items: Vec<MenuItem>, pos: (u16, u16)) -> Self {
        // No leading, trailing or doubled separators.
        items.dedup_by(|a, b| a.separator && b.separator);
        while items.first().is_some_and(|i| i.separator) {
            items.remove(0);
        }
        while items.last().is_some_and(|i| i.separator) {
            items.pop();
        }
        let sel = items.iter().position(MenuItem::selectable).unwrap_or(0);
        Self { title, items, pos, sel, sub: None, rect: Rect::default(), sub_rect: Rect::default() }
    }

    fn sub_items(&self) -> &[MenuItem] {
        self.items.get(self.sel).map_or(&[], |i| i.submenu.as_slice())
    }

    /// Up/down within whichever list is active.
    pub fn step(&mut self, delta: i32) {
        match self.sub {
            Some(s) => self.sub = Some(step(self.sub_items(), s, delta)),
            None => self.sel = step(&self.items, self.sel, delta),
        }
    }

    /// Enter / →: open the submenu or run the item.
    pub fn choose(&mut self) -> Outcome {
        if let Some(s) = self.sub {
            return self.sub_items().get(s).and_then(|i| i.cmd.clone()).map_or(Outcome::Stay, Outcome::Run);
        }
        let Some(item) = self.items.get(self.sel) else { return Outcome::Close };
        if !item.submenu.is_empty() {
            self.sub = Some(step(&item.submenu, usize::MAX, 1));
            return Outcome::Stay;
        }
        item.cmd.clone().map_or(Outcome::Stay, Outcome::Run)
    }

    /// Esc / ←: close the submenu, else the menu.
    pub fn back(&mut self) -> Outcome {
        if self.sub.take().is_some() { Outcome::Stay } else { Outcome::Close }
    }

    /// True if the point is on the menu or its submenu.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        let p = Position { x, y };
        self.rect.contains(p) || (self.sub.is_some() && self.sub_rect.contains(p))
    }

    /// The pointer moved: highlight what's under it (opening submenus).
    pub fn hover(&mut self, x: u16, y: u16) {
        if self.sub.is_some() && self.sub_rect.contains(Position { x, y }) {
            if let Some(i) = row_at(self.sub_rect, self.sub_items(), y) {
                self.sub = Some(i);
            }
            return;
        }
        if self.rect.contains(Position { x, y })
            && let Some(i) = row_at(self.rect, &self.items, y)
        {
            self.sel = i;
            self.sub = (!self.items[i].submenu.is_empty()).then(|| step(&self.items[i].submenu, usize::MAX, 1));
        }
    }

    /// A click on the menu: run the item under the pointer.
    pub fn click(&mut self, x: u16, y: u16) -> Outcome {
        if !self.contains(x, y) {
            return Outcome::Close;
        }
        self.hover(x, y);
        if self.sub_rect.contains(Position { x, y }) && self.sub.is_some() {
            return self.choose();
        }
        if self.items.get(self.sel).is_some_and(|i| !i.submenu.is_empty()) {
            return Outcome::Stay;
        }
        self.sub = None;
        self.choose()
    }
}

fn step(items: &[MenuItem], from: usize, delta: i32) -> usize {
    let n = items.len() as i32;
    if n == 0 {
        return 0;
    }
    let mut i = if from == usize::MAX { -1 } else { from as i32 };
    for _ in 0..n {
        i = (i + delta).rem_euclid(n);
        if items[i as usize].selectable() {
            return i as usize;
        }
    }
    from.min(items.len() - 1)
}

/// The item on screen row `y` of a menu drawn at `rect`.
fn row_at(rect: Rect, items: &[MenuItem], y: u16) -> Option<usize> {
    let first = rect.y + 1 + u16::from(rect.height > items.len() as u16 + 2);
    let i = y.checked_sub(first)? as usize;
    items.get(i).filter(|it| it.selectable()).map(|_| i)
}

fn size(title: Option<&str>, items: &[MenuItem]) -> (u16, u16) {
    let label_w = items.iter().map(|i| i.label.chars().count() + if i.submenu.is_empty() { 0 } else { 2 }).max().unwrap_or(0);
    let key_w = items.iter().map(|i| i.key.chars().count()).max().unwrap_or(0);
    let mut w = label_w + if key_w > 0 { key_w + 4 } else { 0 };
    if let Some(t) = title {
        w = w.max(t.chars().count());
    }
    let h = items.len() + 2 + usize::from(title.is_some());
    (w as u16 + 4, h as u16)
}

/// Place a w×h box at `pos`, flipped to stay on screen.
fn place(pos: (u16, u16), w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    let x = if pos.0 + w > area.right() { pos.0.saturating_sub(w).max(area.x) } else { pos.0 };
    let y = if pos.1 + h > area.bottom() { area.bottom().saturating_sub(h).max(area.y) } else { pos.1 };
    Rect::new(x, y, w, h)
}

pub fn draw(f: &mut Frame, menu: &mut Menu, theme: &Theme) {
    let area = f.area();
    let (w, h) = size(menu.title.as_deref(), &menu.items);
    menu.rect = place(menu.pos, w, h, area);
    draw_list(f, menu.rect, menu.title.as_deref(), &menu.items, Some(menu.sel), menu.sub.is_none(), theme);
    if let Some(s) = menu.sub {
        let items = menu.items[menu.sel].submenu.clone();
        let (w, h) = size(None, &items);
        let item_y = menu.rect.y + 1 + u16::from(menu.title.is_some()) + menu.sel as u16;
        let mut r = place((menu.rect.right(), item_y.saturating_sub(1)), w, h, area);
        if r.x < menu.rect.right() {
            // No room on the right: open to the left.
            r.x = menu.rect.x.saturating_sub(r.width);
        }
        menu.sub_rect = r;
        draw_list(f, r, None, &items, Some(s), true, theme);
    } else {
        menu.sub_rect = Rect::default();
    }
}

/// A filled panel with no frame: a blank cell of padding all round.
fn draw_list(f: &mut Frame, rect: Rect, title: Option<&str>, items: &[MenuItem], sel: Option<usize>, active: bool, t: &Theme) {
    let bg = Style::default().bg(t.menu_bg);
    f.render_widget(Clear, rect);
    f.render_widget(Block::default().style(bg), rect);
    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), rect.height.saturating_sub(2));
    let w = inner.width as usize;
    let mut lines = Vec::new();
    if let Some(title) = title {
        let title: String = title.chars().take(w.saturating_sub(2)).collect();
        lines.push(Line::from(Span::styled(format!(" {title}"), bg.fg(t.muted).add_modifier(Modifier::ITALIC))));
    }
    for (i, item) in items.iter().enumerate() {
        if item.separator {
            lines.push(Line::from(Span::styled("─".repeat(w), bg.fg(t.border))));
            continue;
        }
        let arrow = if item.submenu.is_empty() { "" } else { " ▸" };
        let left = format!(" {}{arrow}", item.label);
        let right = if item.key.is_empty() { String::new() } else { format!("{} ", item.key) };
        let pad = w.saturating_sub(left.chars().count() + right.chars().count());
        let lit = sel == Some(i);
        let base = if lit && active {
            bg.bg(t.menu_highlight).fg(Color::White).add_modifier(Modifier::BOLD)
        } else if lit {
            bg.fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            bg
        };
        let key_style = if lit && active { base } else { bg.fg(t.muted) };
        lines.push(Line::from(vec![
            Span::styled(left, base),
            Span::styled(" ".repeat(pad), base),
            Span::styled(right, key_style),
        ]));
    }
    f.render_widget(Paragraph::new(lines).style(bg), inner);
}

