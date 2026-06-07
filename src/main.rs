mod app;
mod art;
mod audio;
mod config;
mod db;
mod input;
mod ipc;
mod model;
mod playlist;
mod scan;
mod ui;

use std::io::stdout;
use std::sync::mpsc::Receiver;
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
    // Handle remote-control flags before touching the audio device or display.
    if let Some(cmd) = ipc::parse_flag() {
        if let Err(e) = ipc::send_cmd(cmd) {
            eprintln!("music-tui: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }
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

    let cmd_rx = ipc::spawn_listener();

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    // Disambiguate modified keys (e.g. Alt+[ / Alt+]) on terminals that support
    // the kitty keyboard protocol; ignored elsewhere.
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );

    let res = run(&mut terminal, &mut app, cmd_rx);

    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    ipc::cleanup();
    res
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    cmd_rx: Option<Receiver<ipc::Cmd>>,
) -> Result<()> {
    let mut dirty = true; // always draw the first frame
    loop {
        // Poll frequently while playing/scanning so position and progress stay
        // current; also stay responsive when remote commands may arrive.
        let poll_ms =
            if app.audio.is_playing() || app.scanning.is_some() || cmd_rx.is_some() { 100 } else { 500 };
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

        // Drain any incoming remote commands, treating each like a keypress.
        let had_cmd = if let Some(rx) = &cmd_rx {
            let mut any = false;
            while let Ok(cmd) = rx.try_recv() {
                dispatch_cmd(app, cmd);
                any = true;
            }
            any
        } else {
            false
        };

        let tick_dirty = app.on_tick();

        if dirty || had_event || had_cmd || tick_dirty {
            terminal.draw(|f| ui::draw(f, app))?;
            dirty = false;
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn dispatch_cmd(app: &mut App, cmd: ipc::Cmd) {
    match cmd {
        ipc::Cmd::PlayPause => app.toggle_pause(),
        ipc::Cmd::Next => app.next_track(true),
        ipc::Cmd::Previous => app.prev_track(),
        ipc::Cmd::VolumeUp => app.volume_up(),
        ipc::Cmd::VolumeDown => app.volume_down(),
        ipc::Cmd::Repeat => app.cycle_repeat(),
        ipc::Cmd::Shuffle => app.toggle_shuffle(),
    }
}
