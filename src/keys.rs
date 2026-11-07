//! Configurable keyboard shortcuts: named actions bound to key combos.
//! `[keys]` in the config replaces an action's defaults (an empty list
//! unbinds it); `ui.vim_keys` layers h/j/k/l and J/K on top.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Quit,
    ShowKeys,
    Help,
    Search,
    Menu,
    Back,
    Tab1,
    Tab2,
    Tab3,
    Tab4,
    Tab5,
    FocusNext,
    FocusPrev,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    First,
    Last,
    Activate,
    Wider,
    Narrower,
    PlayPause,
    NewPlaylist,
    Next,
    Prev,
    Shuffle,
    Repeat,
    VolumeUp,
    VolumeDown,
    SeekBack,
    SeekForward,
    AppendQueue,
    Queue,
    PlayNext,
    RemoveSong,
    Remove,
    ClearQueue,
    MoveUp,
    MoveDown,
    ToggleQueue,
    Like,
    AddTo,
    GoTo,
    Radio,
    KeepGoing,
    Reject,
    Sort,
    SaveList,
    LinkPlaylist,
    Reimport,
    Art,
}

/// Config name, action, default keys, help group, and description. Where
/// two actions share a key, the earlier one wins wherever it applies (e.g.
/// `n` makes a new playlist in Local Playlists and skips a song elsewhere).
pub const ACTIONS: &[(&str, Action, &[&str], &str, &str)] = &[
    ("quit", Action::Quit, &["q"], "General", "Quit"),
    ("show_keys", Action::ShowKeys, &["?"], "General", "Show keys on screen"),
    ("help", Action::Help, &["F1"], "General", "All shortcuts"),
    ("search", Action::Search, &["/"], "General", "Search"),
    ("menu", Action::Menu, &["Shift+F10", "Menu"], "General", "Menu for the item"),
    ("back", Action::Back, &["Esc", "Backspace"], "General", "Back / clear the search"),
    ("art", Action::Art, &["i"], "General", "Show the cover large"),
    ("tab_1", Action::Tab1, &["1"], "Moving around", "First tab"),
    ("tab_2", Action::Tab2, &["2"], "Moving around", "Second tab"),
    ("tab_3", Action::Tab3, &["3"], "Moving around", "Third tab"),
    ("tab_4", Action::Tab4, &["4"], "Moving around", "Fourth tab"),
    ("tab_5", Action::Tab5, &["5"], "Moving around", "Fifth tab"),
    ("focus_next", Action::FocusNext, &["Tab"], "Moving around", "Next pane or control"),
    ("focus_prev", Action::FocusPrev, &["Shift+Tab"], "Moving around", "Previous pane or control"),
    ("up", Action::Up, &["Up"], "Moving around", "Up"),
    ("down", Action::Down, &["Down"], "Moving around", "Down"),
    ("left", Action::Left, &["Left"], "Moving around", "Left pane"),
    ("right", Action::Right, &["Right"], "Moving around", "Right pane"),
    ("page_up", Action::PageUp, &["PageUp"], "Moving around", "Up a page"),
    ("page_down", Action::PageDown, &["PageDown"], "Moving around", "Down a page"),
    ("first", Action::First, &["Home"], "Moving around", "First item"),
    ("last", Action::Last, &["End"], "Moving around", "Last item"),
    ("activate", Action::Activate, &["Enter"], "Moving around", "Play / open"),
    ("wider", Action::Wider, &["Alt+]"], "Moving around", "Widen the pane"),
    ("narrower", Action::Narrower, &["Alt+["], "Moving around", "Narrow the pane"),
    ("play_pause", Action::PlayPause, &["Space"], "Playback", "Play / pause"),
    ("new_playlist", Action::NewPlaylist, &["n"], "Local Playlists", "New playlist from the queue"),
    ("next", Action::Next, &["n"], "Playback", "Next song"),
    ("prev", Action::Prev, &["p"], "Playback", "Previous song"),
    ("shuffle", Action::Shuffle, &["s"], "Playback", "Shuffle"),
    ("repeat", Action::Repeat, &["r"], "Playback", "Repeat"),
    ("volume_up", Action::VolumeUp, &["+", "="], "Playback", "Volume up"),
    ("volume_down", Action::VolumeDown, &["-", "_"], "Playback", "Volume down"),
    ("seek_back", Action::SeekBack, &["["], "Playback", "Back 5 seconds"),
    ("seek_forward", Action::SeekForward, &["]"], "Playback", "Forward 5 seconds"),
    ("keep_going", Action::KeepGoing, &["G"], "Playback", "Keep playing similar songs"),
    ("append_queue", Action::AppendQueue, &["a"], "Local Playlists", "Add the queue to the playlist"),
    ("queue", Action::Queue, &["a"], "Songs", "Add to queue"),
    ("play_next", Action::PlayNext, &["A"], "Songs", "Play next"),
    ("like", Action::Like, &["l", "f"], "Songs", "Like / unlike"),
    ("add_to", Action::AddTo, &["m"], "Songs", "Add to playlist"),
    ("go_to", Action::GoTo, &["g"], "Songs", "Go to (radio, artist, album)"),
    ("radio", Action::Radio, &[], "Songs", "Song radio"),
    ("remove_song", Action::RemoveSong, &["x"], "Local Playlists", "Remove the song"),
    ("reject", Action::Reject, &["x"], "Songs", "Wrong song? Find another"),
    ("remove", Action::Remove, &["d", "Delete"], "Songs", "Remove"),
    ("sort", Action::Sort, &["o"], "Songs", "Change the sort"),
    ("save_list", Action::SaveList, &["S"], "Songs", "Save as a playlist"),
    ("link_playlist", Action::LinkPlaylist, &["u"], "Songs", "Link a Spotify playlist"),
    ("reimport", Action::Reimport, &[], "Songs", "Re-read the Spotify export"),
    ("toggle_queue", Action::ToggleQueue, &["Q"], "Queue", "Show / hide the queue"),
    ("clear_queue", Action::ClearQueue, &["c"], "Queue", "Clear the queue"),
    ("move_up", Action::MoveUp, &["Shift+Up"], "Queue", "Move the song up"),
    ("move_down", Action::MoveDown, &["Shift+Down"], "Queue", "Move the song down"),
];

/// Extra keys with `ui.vim_keys` on. They come before the defaults, so `l`
/// moves right instead of liking (`f` still likes).
pub const VIM: &[(Action, &[&str])] = &[
    (Action::Up, &["k"]),
    (Action::Down, &["j"]),
    (Action::Left, &["h"]),
    (Action::Right, &["l"]),
    (Action::MoveUp, &["K"]),
    (Action::MoveDown, &["J"]),
];

/// A key combo, normalized so a parsed spec and a key event compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Combo {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Combo {
    fn new(code: KeyCode, mods: KeyModifiers) -> Self {
        let mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER);
        match code {
            // Letters compare lowercase with Shift; for symbols, Shift is
            // part of which symbol it is ("?" not "Shift+/").
            KeyCode::Char(c) if c.is_ascii_alphabetic() => {
                let mods = if c.is_ascii_uppercase() { mods | KeyModifiers::SHIFT } else { mods };
                Self { code: KeyCode::Char(c.to_ascii_lowercase()), mods }
            }
            KeyCode::Char(c) => Self { code: KeyCode::Char(c), mods: mods - KeyModifiers::SHIFT },
            KeyCode::BackTab => Self { code: KeyCode::Tab, mods: mods | KeyModifiers::SHIFT },
            code => Self { code, mods },
        }
    }

    pub fn from_event(ev: &KeyEvent) -> Self {
        Self::new(ev.code, ev.modifiers)
    }

    /// Parse "Ctrl+Shift+X", "Alt+]", "F10", "Backspace", "Q" (Shift+Q).
    pub fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        // A trailing "+" is the plus key itself ("Ctrl++", "+").
        let (mods_part, key) = match spec.strip_suffix("++") {
            Some(m) => (m, "+"),
            None if spec == "+" => ("", "+"),
            None => match spec.rsplit_once('+') {
                Some((m, k)) if !k.is_empty() => (m, k),
                _ => ("", spec),
            },
        };
        let mut mods = KeyModifiers::NONE;
        for m in mods_part.split('+').filter(|m| !m.is_empty()) {
            mods |= match m.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "meta" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                "super" | "win" => KeyModifiers::SUPER,
                _ => return None,
            };
        }
        let code = match key.to_ascii_lowercase().as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "tab" => KeyCode::Tab,
            "space" => KeyCode::Char(' '),
            "plus" => KeyCode::Char('+'),
            "minus" => KeyCode::Char('-'),
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdown" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "menu" => KeyCode::Menu,
            f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => KeyCode::F(f[1..].parse().ok()?),
            _ => {
                let mut chars = key.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                // A bare uppercase letter is that letter with Shift ("Q");
                // with other modifiers, Shift has to be spelled out.
                if mods.is_empty() { KeyCode::Char(c) } else { KeyCode::Char(c.to_ascii_lowercase()) }
            }
        };
        Some(Self::new(code, mods))
    }

    /// Short display form: "a", "A" (Shift+A), "Space", "Shift+↑", "Alt+]".
    pub fn label(&self) -> String {
        let shift = self.mods.contains(KeyModifiers::SHIFT);
        let letter = matches!(self.code, KeyCode::Char(c) if c.is_ascii_alphabetic());
        let mut s = String::new();
        for (m, name) in [(KeyModifiers::CONTROL, "Ctrl+"), (KeyModifiers::ALT, "Alt+"), (KeyModifiers::SUPER, "Super+")] {
            if self.mods.contains(m) {
                s.push_str(name);
            }
        }
        if shift && !letter {
            s.push_str("Shift+");
        }
        match self.code {
            KeyCode::Char(' ') => s.push_str("Space"),
            KeyCode::Char(c) if shift => s.push(c.to_ascii_uppercase()),
            KeyCode::Char(c) => s.push(c),
            KeyCode::F(n) => s.push_str(&format!("F{n}")),
            KeyCode::Up => s.push('↑'),
            KeyCode::Down => s.push('↓'),
            KeyCode::Left => s.push('←'),
            KeyCode::Right => s.push('→'),
            KeyCode::Enter => s.push_str("Enter"),
            KeyCode::Backspace => s.push('⌫'),
            KeyCode::Delete => s.push_str("Del"),
            KeyCode::PageUp => s.push_str("PgUp"),
            KeyCode::PageDown => s.push_str("PgDn"),
            code => s.push_str(&format!("{code:?}")),
        }
        s
    }
}

pub struct Keymap {
    map: HashMap<Combo, Vec<Action>>,
    /// Keys per action, in table order, for labels and the help screen.
    bound: Vec<(Action, Vec<Combo>)>,
}

impl Keymap {
    /// Defaults (plus the vim layer) overridden by the config's `[keys]`;
    /// returns the problems (unknown actions or keys) to report.
    pub fn new(overrides: &HashMap<String, Vec<String>>, vim: bool) -> (Self, Vec<String>) {
        let mut problems = Vec::new();
        for name in overrides.keys() {
            if !ACTIONS.iter().any(|(n, ..)| n == name) {
                problems.push(format!("unknown action \"{name}\""));
            }
        }
        let mut map: HashMap<Combo, Vec<Action>> = HashMap::new();
        let mut bound: Vec<(Action, Vec<Combo>)> = Vec::new();
        if vim {
            for &(action, keys) in VIM {
                for c in keys.iter().filter_map(|k| Combo::parse(k)) {
                    map.entry(c).or_default().push(action);
                    bound.push((action, vec![c]));
                }
            }
        }
        for &(name, action, defaults, ..) in ACTIONS {
            let specs: Vec<&str> = match overrides.get(name) {
                Some(v) => v.iter().map(String::as_str).collect(),
                None => defaults.to_vec(),
            };
            let mut combos = Vec::new();
            for spec in specs {
                match Combo::parse(spec) {
                    Some(c) => {
                        map.entry(c).or_default().push(action);
                        combos.push(c);
                    }
                    None => problems.push(format!("{name}: can't read key \"{spec}\"")),
                }
            }
            bound.push((action, combos));
        }
        (Self { map, bound }, problems)
    }

    /// Every action bound to the key, in priority order.
    pub fn actions(&self, ev: &KeyEvent) -> &[Action] {
        self.map.get(&Combo::from_event(ev)).map_or(&[], Vec::as_slice)
    }

    /// True if `ev` is bound to `action`.
    pub fn is(&self, ev: &KeyEvent, action: Action) -> bool {
        self.actions(ev).contains(&action)
    }

    /// All keys for an action, vim keys first.
    pub fn keys(&self, action: Action) -> Vec<Combo> {
        self.bound.iter().filter(|(a, _)| *a == action).flat_map(|(_, c)| c.iter().copied()).collect()
    }

    /// The main key for an action, for menus and hints ("" when unbound).
    pub fn label(&self, action: Action) -> String {
        self.keys(action).first().map(Combo::label).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn every_default_key_parses() {
        let (_, problems) = Keymap::new(&HashMap::new(), true);
        assert!(problems.is_empty(), "{problems:?}");
        for (name, _, keys, ..) in ACTIONS {
            for spec in *keys {
                assert!(Combo::parse(spec).is_some(), "{name}: {spec}");
            }
        }
    }

    #[test]
    fn shared_keys_keep_table_order() {
        let (km, _) = Keymap::new(&HashMap::new(), false);
        let n = ev(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(km.actions(&n), &[Action::NewPlaylist, Action::Next]);
        let l = ev(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(km.actions(&l), &[Action::Like]);
        let (vim, _) = Keymap::new(&HashMap::new(), true);
        assert_eq!(vim.actions(&l), &[Action::Right, Action::Like]);
        assert_eq!(vim.label(Action::Up), "k");
        assert_eq!(km.label(Action::Up), "↑");
    }

    #[test]
    fn parse_match_and_label() {
        let (km, problems) = Keymap::new(
            &HashMap::from([
                ("like".to_string(), vec!["Ctrl+L".to_string()]),
                ("sort".to_string(), vec![]),
                ("bogus".to_string(), vec!["x".to_string()]),
            ]),
            false,
        );
        assert_eq!(problems, vec!["unknown action \"bogus\"".to_string()]);
        assert!(km.is(&ev(KeyCode::Char('l'), KeyModifiers::CONTROL), Action::Like));
        assert!(km.actions(&ev(KeyCode::Char('f'), KeyModifiers::NONE)).is_empty());
        assert!(km.actions(&ev(KeyCode::Char('o'), KeyModifiers::NONE)).is_empty());
        // Shifted letters arrive uppercase, with or without the flag.
        assert!(km.is(&ev(KeyCode::Char('Q'), KeyModifiers::SHIFT), Action::ToggleQueue));
        assert!(km.is(&ev(KeyCode::Char('Q'), KeyModifiers::NONE), Action::ToggleQueue));
        assert!(km.is(&ev(KeyCode::Char('?'), KeyModifiers::SHIFT), Action::ShowKeys));
        assert!(km.is(&ev(KeyCode::BackTab, KeyModifiers::SHIFT), Action::FocusPrev));
        assert!(km.is(&ev(KeyCode::Char('+'), KeyModifiers::NONE), Action::VolumeUp));
        assert!(km.is(&ev(KeyCode::Char(']'), KeyModifiers::ALT), Action::Wider));
        assert_eq!(km.label(Action::ToggleQueue), "Q");
        assert_eq!(km.label(Action::PlayPause), "Space");
        assert_eq!(km.label(Action::MoveUp), "Shift+↑");
        assert_eq!(km.label(Action::Wider), "Alt+]");
        assert_eq!(km.label(Action::Radio), "");
    }
}
