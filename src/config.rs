use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use directories::{ProjectDirs, UserDirs};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Root directory scanned for music. Defaults to ~/Music.
    #[serde(default = "default_music_dir")]
    pub music_dir: PathBuf,
    /// Color theme. Missing/old config files fall back to the defaults.
    #[serde(default)]
    pub theme: ThemeConfig,
    /// Persisted browser column widths.
    #[serde(default)]
    pub layout: LayoutConfig,
    /// Playback options.
    #[serde(default)]
    pub playback: PlaybackConfig,
    /// UI options.
    #[serde(default)]
    pub ui: UiConfig,
}

/// UI options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Render transport icons at 2x using the kitty text-sizing protocol
    /// (kitty ≥0.40). Set false on terminals without it.
    #[serde(default = "default_true")]
    pub big_transport_icons: bool,
    /// Pre-cache cover-art thumbnails for the whole library in the background.
    #[serde(default)]
    pub cache_all_art: bool,
    /// Sort artists that start with "The" by the remainder, e.g. "The Doors"
    /// sorts and displays as "Doors, The".
    #[serde(default = "default_true")]
    pub sort_articles: bool,
}

fn default_true() -> bool {
    true
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            big_transport_icons: default_true(),
            cache_all_art: false,
            sort_articles: true,
        }
    }
}

/// Playback options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackConfig {
    /// Gapless playback: crossfade duration in seconds between tracks (0 = off).
    /// Accepts the old `crossfade_secs` name for backwards compatibility.
    #[serde(default = "default_gapless", alias = "crossfade_secs")]
    pub gapless_fade_secs: f64,
}

fn default_gapless() -> f64 {
    3.0
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            gapless_fade_secs: default_gapless(),
        }
    }
}

/// Persisted layout state (remembered across runs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    /// Width percentages for the three browser columns; should sum to ~100.
    #[serde(default = "default_columns")]
    pub columns: Vec<u16>,
    /// Height percentage of the queue panel within the third column.
    #[serde(default = "default_queue_pct")]
    pub queue_pct: u16,
}

fn default_columns() -> Vec<u16> {
    vec![25, 30, 45]
}

fn default_queue_pct() -> u16 {
    28
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            columns: default_columns(),
            queue_pct: default_queue_pct(),
        }
    }
}

impl Config {
    /// Gapless crossfade duration clamped to a sane range.
    pub fn crossfade_secs(&self) -> f32 {
        self.playback.gapless_fade_secs.clamp(0.0, 12.0) as f32
    }

    /// Column widths as a validated, renormalized `[left, middle, right]` that
    /// always sums to 100 and keeps every column at a usable minimum.
    pub fn validated_columns(&self) -> [u16; 3] {
        let c = &self.layout.columns;
        let mut a = if c.len() == 3 {
            [c[0], c[1], c[2]]
        } else {
            [25, 30, 45]
        };
        for v in &mut a {
            *v = (*v).clamp(10, 80);
        }
        let sum: u32 = a.iter().map(|x| *x as u32).sum();
        for v in &mut a {
            *v = ((*v as u32 * 100) / sum) as u16;
        }
        let s: u16 = a.iter().sum();
        a[2] = a[2].saturating_add(100u16.saturating_sub(s));
        a
    }

    /// Queue panel height percentage, clamped to a usable range.
    pub fn validated_queue_pct(&self) -> u16 {
        self.layout.queue_pct.clamp(10, 60)
    }
}

/// Documented `[theme]` block appended to configs that predate theming, and
/// included when a fresh config file is created.
const THEME_TEMPLATE: &str = "\n[theme]\n\
# Colors accept a name (\"cyan\", \"lightmagenta\"), hex (\"#ff8800\"),\n\
# or a 256-color index (\"39\"). Restart music-tui to apply changes.\n\
accent = \"cyan\"         # highlights, focused borders, progress & volume\n\
selection_fg = \"black\"  # text drawn on the accent background\n\
border = \"darkgray\"     # unfocused panel borders\n\
dim = \"darkgray\"        # years, track numbers, hints\n\
muted = \"gray\"          # artist subtext, status line\n\
\n[playback]\n\
# Gapless playback: crossfade seconds between tracks (0 = hard cut).\n\
gapless_fade_secs = 3.0\n\
\n[ui]\n\
# Render transport icons at 2x (needs kitty >= 0.40).\n\
big_transport_icons = true\n\
# Pre-cache all cover-art thumbnails in the background.\n\
cache_all_art = false\n\
# Display \"The Doors\" as \"Doors, The\" and sort accordingly.\n\
sort_articles = true\n";

impl Default for Config {
    fn default() -> Self {
        Self {
            music_dir: default_music_dir(),
            theme: ThemeConfig::default(),
            layout: LayoutConfig::default(),
            playback: PlaybackConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

/// Colors as written in config.toml. Each value accepts a named color
/// ("cyan", "lightmagenta"), a hex string ("#ff8800"), or an indexed color
/// number ("39"). Invalid values fall back to the corresponding default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "d_accent")]
    pub accent: String,
    #[serde(default = "d_selection_fg")]
    pub selection_fg: String,
    #[serde(default = "d_border")]
    pub border: String,
    #[serde(default = "d_dim")]
    pub dim: String,
    #[serde(default = "d_muted")]
    pub muted: String,
}

fn d_accent() -> String {
    "cyan".into()
}
fn d_selection_fg() -> String {
    "black".into()
}
fn d_border() -> String {
    "darkgray".into()
}
fn d_dim() -> String {
    "darkgray".into()
}
fn d_muted() -> String {
    "gray".into()
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            accent: d_accent(),
            selection_fg: d_selection_fg(),
            border: d_border(),
            dim: d_dim(),
            muted: d_muted(),
        }
    }
}

/// Resolved theme with parsed `ratatui` colors, cheap to copy each frame.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub accent: Color,
    pub selection_fg: Color,
    pub border: Color,
    pub dim: Color,
    pub muted: Color,
}

impl ThemeConfig {
    pub fn resolve(&self) -> Theme {
        Theme {
            accent: parse_color(&self.accent, Color::Cyan),
            selection_fg: parse_color(&self.selection_fg, Color::Black),
            border: parse_color(&self.border, Color::DarkGray),
            dim: parse_color(&self.dim, Color::DarkGray),
            muted: parse_color(&self.muted, Color::Gray),
        }
    }
}

fn parse_color(s: &str, fallback: Color) -> Color {
    Color::from_str(s.trim()).unwrap_or(fallback)
}

fn default_music_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|u| u.audio_dir().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            UserDirs::new()
                .map(|u| u.home_dir().join("Music"))
                .unwrap_or_else(|| PathBuf::from("Music"))
        })
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", "music-tui")
}

/// Path to the SQLite library index, e.g. ~/.local/share/music-tui/library.db.
pub fn db_path() -> PathBuf {
    project_dirs()
        .map(|d| d.data_dir().join("library.db"))
        .unwrap_or_else(|| PathBuf::from("music-tui-library.db"))
}

/// Path to the saved playlists file, e.g. ~/.local/share/music-tui/playlists.toml.
pub fn playlists_path() -> PathBuf {
    project_dirs()
        .map(|d| d.data_dir().join("playlists.toml"))
        .unwrap_or_else(|| PathBuf::from("music-tui-playlists.toml"))
}

/// Directory for cached cover-art thumbnails, e.g. ~/.cache/music-tui/art.
pub fn art_cache_dir() -> PathBuf {
    project_dirs()
        .map(|d| d.cache_dir().join("art"))
        .unwrap_or_else(|| PathBuf::from("music-tui-art-cache"))
}

fn config_path() -> PathBuf {
    project_dirs()
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("music-tui-config.toml"))
}

/// The config file path as a display string, shown in the Settings tab.
pub fn config_path_display() -> String {
    config_path().to_string_lossy().into_owned()
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match fs::read_to_string(&path) {
            Ok(s) => {
                let cfg: Config = toml::from_str(&s).unwrap_or_default();
                // Existing config from before themes: append a documented
                // [theme] section without disturbing the user's content/comments.
                if !s.contains("[theme]") {
                    let mut s = s;
                    if !s.ends_with('\n') {
                        s.push('\n');
                    }
                    s.push_str(THEME_TEMPLATE);
                    let _ = fs::write(&path, s);
                }
                cfg
            }
            Err(_) => {
                // First run: write a documented default config so the user has a
                // [theme] section to edit.
                let cfg = Config::default();
                let escaped = cfg
                    .music_dir
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                let body = format!("music_dir = \"{escaped}\"\n{THEME_TEMPLATE}");
                let _ = ensure_parent(&path);
                let _ = fs::write(&path, body);
                cfg
            }
        }
    }

    /// Write the config back, preserving the user's comments and any keys we
    /// don't manage (via `toml_edit` read-modify-write).
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        ensure_parent(&path)?;
        let existing = fs::read_to_string(&path).unwrap_or_default();
        fs::write(&path, self.render_toml(&existing))?;
        Ok(())
    }

    /// Pure read-modify-write of a config document: updates the keys we manage
    /// while leaving comments and unrelated keys intact. A key is only rewritten
    /// when its value actually changed, so inline comments survive.
    fn render_toml(&self, existing: &str) -> String {
        let mut doc = existing
            .parse::<toml_edit::DocumentMut>()
            .unwrap_or_default();

        set_str(doc.as_table_mut(), "music_dir", &self.music_dir.to_string_lossy());

        if let Some(theme) = doc["theme"].or_insert(toml_edit::table()).as_table_mut() {
            set_str(theme, "accent", &self.theme.accent);
            set_str(theme, "selection_fg", &self.theme.selection_fg);
            set_str(theme, "border", &self.theme.border);
            set_str(theme, "dim", &self.theme.dim);
            set_str(theme, "muted", &self.theme.muted);
        }

        let want: Vec<i64> = self.layout.columns.iter().map(|c| *c as i64).collect();
        if let Some(layout) = doc["layout"].or_insert(toml_edit::table()).as_table_mut() {
            let current: Option<Vec<i64>> = layout
                .get("columns")
                .and_then(|i| i.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_integer()).collect());
            if current.as_deref() != Some(want.as_slice()) {
                let mut arr = toml_edit::Array::new();
                for c in &want {
                    arr.push(*c);
                }
                layout["columns"] = toml_edit::value(arr);
            }
            if layout.get("queue_pct").and_then(|i| i.as_integer())
                != Some(self.layout.queue_pct as i64)
            {
                layout["queue_pct"] = toml_edit::value(self.layout.queue_pct as i64);
            }
        }

        if let Some(playback) = doc["playback"].or_insert(toml_edit::table()).as_table_mut() {
            if playback.get("gapless_fade_secs").and_then(|i| i.as_float())
                != Some(self.playback.gapless_fade_secs)
            {
                playback["gapless_fade_secs"] = toml_edit::value(self.playback.gapless_fade_secs);
            }
            playback.remove("crossfade_secs"); // migrate the old key name
        }

        if let Some(ui) = doc["ui"].or_insert(toml_edit::table()).as_table_mut() {
            if ui.get("big_transport_icons").and_then(|i| i.as_bool())
                != Some(self.ui.big_transport_icons)
            {
                ui["big_transport_icons"] = toml_edit::value(self.ui.big_transport_icons);
            }
            if ui.get("cache_all_art").and_then(|i| i.as_bool()) != Some(self.ui.cache_all_art) {
                ui["cache_all_art"] = toml_edit::value(self.ui.cache_all_art);
            }
            if ui.get("sort_articles").and_then(|i| i.as_bool()) != Some(self.ui.sort_articles) {
                ui["sort_articles"] = toml_edit::value(self.ui.sort_articles);
            }
        }

        doc.to_string()
    }
}

/// Set a string key only if it differs, so existing inline comments are kept.
fn set_str(tbl: &mut toml_edit::Table, key: &str, val: &str) {
    if tbl.get(key).and_then(|i| i.as_str()) != Some(val) {
        tbl[key] = toml_edit::value(val);
    }
}

/// Ensure the parent directory of a path exists.
pub fn ensure_parent(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_default_file_parses() {
        let body = format!("music_dir = \"/home/u/Music\"\n{THEME_TEMPLATE}");
        let cfg: Config = toml::from_str(&body).expect("default config template must be valid TOML");
        assert_eq!(cfg.music_dir, PathBuf::from("/home/u/Music"));
        assert_eq!(cfg.theme.accent, "cyan");
        // Named, hex, and indexed colors all resolve without panicking.
        assert_eq!(cfg.theme.resolve().accent, Color::Cyan);
    }

    #[test]
    fn old_config_without_theme_uses_defaults() {
        let cfg: Config = toml::from_str("music_dir = \"/x\"").unwrap();
        assert_eq!(cfg.theme.muted, "gray");
    }

    #[test]
    fn hex_and_indexed_colors_parse() {
        assert_eq!(parse_color("#ff8800", Color::Cyan), Color::Rgb(0xff, 0x88, 0x00));
        assert_eq!(parse_color("39", Color::Cyan), Color::Indexed(39));
        assert_eq!(parse_color("not-a-color", Color::Magenta), Color::Magenta);
    }

    #[test]
    fn save_preserves_comments_and_updates_keys() {
        let existing = "\
# my notes
music_dir = \"/old/path\"

[theme]
accent = \"cyan\"  # I like cyan
";
        // Real saves are triggered by dir/layout edits; theme keys are loaded
        // unchanged, so their inline comments must survive a save.
        let mut cfg = Config::default();
        cfg.music_dir = PathBuf::from("/new/path");
        cfg.theme.accent = "cyan".into(); // unchanged
        cfg.layout.columns = vec![20, 30, 50];

        let out = cfg.render_toml(existing);
        assert!(out.contains("# my notes"), "top comment preserved");
        assert!(out.contains("# I like cyan"), "unchanged key keeps its comment");
        assert!(out.contains("/new/path"), "music_dir updated");
        assert!(out.contains("[layout]") && out.contains("20"), "layout written");

        // And it round-trips back into a Config.
        let reparsed: Config = toml::from_str(&out).unwrap();
        assert_eq!(reparsed.validated_columns(), [20, 30, 50]);
    }

    #[test]
    fn validated_columns_renormalizes_garbage() {
        let mut cfg = Config::default();
        cfg.layout.columns = vec![999]; // wrong length -> defaults
        assert_eq!(cfg.validated_columns().iter().sum::<u16>(), 100);
        cfg.layout.columns = vec![50, 50, 50]; // sums >100 -> renormalized
        assert_eq!(cfg.validated_columns().iter().sum::<u16>(), 100);
    }
}
