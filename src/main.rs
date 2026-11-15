mod app;
mod art;
mod audio;
mod config;
mod db;
mod deezer;
mod opus;
mod input;
mod keys;
mod menu;
mod ipc;
mod model;
mod mpris;
mod playlist;
mod podcast;
mod radio;
mod scan;
mod serve;
mod spotify;
mod stream;
mod sync;
mod ui;

use std::io::stdout;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{channel, Receiver};
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
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("serve") {
        return serve::main(&args[2..]);
    }
    // Handle remote-control flags before touching the audio device or display.
    if let Some(cmd) = ipc::parse_flag() {
        if let Err(e) = ipc::send_cmd(&cmd) {
            // Not running: a like can go straight into the saved library.
            if let ipc::Cmd::Like(song) = &cmd {
                spotify::like_offline(song);
                return Ok(());
            }
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

    let (cmd_tx, cmd_rx) = channel();
    let listening = ipc::spawn_listener(cmd_tx.clone());
    let mpris = mpris::Mpris::new(cmd_tx);
    let remote = listening || mpris.is_some();

    let std_hook = std::panic::take_hook();
    let mut terminal = ratatui::init();
    stderr_to_log();
    // A panic on the main thread ends the TUI: put the terminal and stderr
    // back first so the message shows. One on a background thread (say, the
    // MPRIS service) only goes to the log, leaving the TUI running intact.
    let tui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            restore_stderr();
            tui_hook(info);
        } else {
            std_hook(info);
        }
    }));
    execute!(stdout(), EnableMouseCapture)?;
    // Disambiguate modified keys (e.g. Alt+[ / Alt+]) on terminals that support
    // the kitty keyboard protocol; ignored elsewhere.
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );

    let res = run(&mut terminal, &mut app, cmd_rx, mpris, remote);
    app.save_session();
    app.sync_on_quit();

    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    restore_stderr();
    ipc::cleanup();
    res
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    cmd_rx: Receiver<ipc::Cmd>,
    mut mpris: Option<mpris::Mpris>,
    remote: bool,
) -> Result<()> {
    let mut dirty = true; // always draw the first frame
    loop {
        // Poll frequently while playing/scanning so position and progress stay
        // current; also stay responsive when remote commands may arrive.
        let busy = app.audio.is_playing()
            || app.scanning.is_some()
            || !app.fetching.is_empty()
            || remote;
        // Covers still loading: check back quickly so each shows as soon as
        // it's decoded.
        let poll_ms = if app.thumbs_pending() { 30 } else if busy { 100 } else { 500 };
        let had_event = if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    input::handle_key(app, k);
                    true
                }
                // Plain pointer motion only matters to an open menu.
                Event::Mouse(m) if m.kind == event::MouseEventKind::Moved && app.menu.is_none() => false,
                Event::Mouse(m) => {
                    input::handle_mouse(app, m);
                    true
                }
                Event::Resize(_, _) => {
                    app.on_resize();
                    dirty = true;
                    true
                }
                _ => true,
            }
        } else {
            false
        };

        // Drain any incoming remote commands, treating each like a keypress.
        let mut had_cmd = false;
        while let Ok(cmd) = cmd_rx.try_recv() {
            dispatch_cmd(app, cmd);
            had_cmd = true;
        }

        let tick_dirty = app.on_tick();
        if let Some(m) = &mut mpris {
            m.update(app);
        }

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
        ipc::Cmd::Play => {
            if !app.audio.is_playing() {
                app.toggle_pause();
            }
        }
        ipc::Cmd::Pause => {
            if app.audio.is_playing() {
                app.toggle_pause();
            }
        }
        ipc::Cmd::SeekBy(secs) => app.seek_relative(secs),
        ipc::Cmd::SeekTo(pos) => {
            app.audio.seek_to(pos);
        }
        ipc::Cmd::SetVolume(v) => app.set_volume(v),
        ipc::Cmd::Like(song) => app.like_song(&song),
    }
}

/// The terminal's stderr, saved while it points at the log.
static SAVED_STDERR: AtomicI32 = AtomicI32::new(-1);

/// While the TUI is up, stderr goes to a log file. Whatever libraries print
/// (warnings, ALSA messages, background panics) would otherwise land wherever
/// the cursor is, and a newline on the bottom row scrolls the whole screen.
fn stderr_to_log() {
    let path = config::stderr_log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    // SAFETY: plain fd calls; `file` stays open until dup2 has copied it.
    unsafe {
        let saved = libc::dup(2);
        if saved < 0 {
            return;
        }
        if libc::dup2(file.as_raw_fd(), 2) < 0 {
            libc::close(saved);
            return;
        }
        SAVED_STDERR.store(saved, Ordering::SeqCst);
    }
}

/// Point stderr back at the terminal.
fn restore_stderr() {
    let saved = SAVED_STDERR.swap(-1, Ordering::SeqCst);
    if saved >= 0 {
        // SAFETY: `saved` is the descriptor dup'd in `stderr_to_log`.
        unsafe {
            libc::dup2(saved, 2);
            libc::close(saved);
        }
    }
}
