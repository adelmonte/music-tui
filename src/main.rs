mod app;
mod art;
mod audio;
mod config;
mod db;
mod input;
mod model;
mod playlist;
mod scan;
mod ui;

use std::io::stdout;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;

use app::App;
use art::Art;
use audio::Audio;
use config::Config;
use model::Library;

fn main() -> Result<()> {
    // Set MUSIC_TUI_TIMING=1 to print per-phase startup timings to stderr (shown
    // on the normal screen, before the alternate screen is entered).
    let timing = std::env::var_os("MUSIC_TUI_TIMING").is_some();
    let t0 = Instant::now();

    let config = Config::load();

    // Load the existing index (fast startup); empty library if none yet.
    let library = match db::open() {
        Ok(conn) => db::load_all(&conn).map(Library::new).unwrap_or_default(),
        Err(_) => Library::default(),
    };
    if timing {
        eprintln!("[timing] config + db load + index : {:?}", t0.elapsed());
    }

    // Detect terminal graphics capability *before* entering the alternate screen,
    // since the query talks to stdio.
    let t = Instant::now();
    let art = Art::new();
    if timing {
        eprintln!("[timing] graphics query (Art::new) : {:?}", t.elapsed());
    }

    let t = Instant::now();
    let audio = Audio::new(config.crossfade_secs())?;
    if timing {
        eprintln!("[timing] audio device init         : {:?}", t.elapsed());
    }

    let t = Instant::now();
    let mut app = App::new(config, library, audio, art);
    if timing {
        eprintln!("[timing] App::new (first refresh)  : {:?}", t.elapsed());
        eprintln!("[timing] total startup             : {:?}", t0.elapsed());
    }

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    // Disambiguate modified keys (e.g. Alt+[ / Alt+]) on terminals that support
    // the kitty keyboard protocol; ignored elsewhere.
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );

    let res = run(&mut terminal, &mut app);

    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut dirty = true; // always draw the first frame
    loop {
        // Poll frequently while playing/scanning so position and progress stay
        // current; drop to a long sleep when idle — events still wake us instantly.
        let poll_ms = if app.audio.is_playing() || app.scanning.is_some() { 100 } else { 500 };
        let had_event = if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => input::handle_key(app, k),
                Event::Mouse(m) => input::handle_mouse(app, m),
                Event::Resize(_, _) => {
                    app.on_resize();
                    dirty = true;
                }
                _ => {}
            }
            true
        } else {
            false
        };

        let tick_dirty = app.on_tick();

        if dirty || had_event || tick_dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
