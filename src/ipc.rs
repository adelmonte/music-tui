use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use anyhow::{Context, Result};

pub enum Cmd {
    PlayPause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    Repeat,
    Shuffle,
}

impl Cmd {
    fn from_str(s: &str) -> Option<Self> {
        match s.trim() {
            "play-pause" => Some(Self::PlayPause),
            "next" => Some(Self::Next),
            "previous" => Some(Self::Previous),
            "volume-up" => Some(Self::VolumeUp),
            "volume-down" => Some(Self::VolumeDown),
            "repeat" => Some(Self::Repeat),
            "shuffle" => Some(Self::Shuffle),
            _ => None,
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::PlayPause => "play-pause",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::VolumeUp => "volume-up",
            Self::VolumeDown => "volume-down",
            Self::Repeat => "repeat",
            Self::Shuffle => "shuffle",
        }
    }
}

fn socket_path() -> PathBuf {
    // Prefer XDG_RUNTIME_DIR (e.g. /run/user/1000) — the standard place for
    // transient sockets on Linux.
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("music-tui.sock");
    }
    // Fall back to the app's data directory.
    if let Some(dirs) = directories::ProjectDirs::from("", "", "music-tui") {
        let d = dirs.data_local_dir().to_path_buf();
        let _ = std::fs::create_dir_all(&d);
        return d.join("cmd.sock");
    }
    PathBuf::from("/tmp/music-tui.sock")
}

/// Send a single command to the running music-tui instance and return.
pub fn send_cmd(cmd: Cmd) -> Result<()> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)
        .context("music-tui does not appear to be running")?;
    writeln!(stream, "{}", cmd.as_str())?;
    Ok(())
}

/// Bind the command socket and return a channel receiver for incoming commands.
/// Removes any stale socket file first (crash recovery). Returns `None` if the
/// socket cannot be bound (non-fatal; the TUI starts without remote control).
pub fn spawn_listener() -> Option<Receiver<Cmd>> {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).ok()?;
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().flatten() {
                    if let Some(cmd) = Cmd::from_str(&line) {
                        let _ = tx.send(cmd);
                    }
                }
            });
        }
    });
    Some(rx)
}

/// Remove the socket file. Called on clean shutdown.
pub fn cleanup() {
    let _ = std::fs::remove_file(socket_path());
}

/// Parse the first recognized `--flag` from the command-line arguments.
/// Returns `Some(cmd)` if a control flag was found, `None` to start normally.
/// Prints usage and exits on `--help` or an unrecognized `--` flag.
pub fn parse_flag() -> Option<Cmd> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    for arg in &args {
        match arg.as_str() {
            "--play-pause" => return Some(Cmd::PlayPause),
            "--next" => return Some(Cmd::Next),
            "--previous" => return Some(Cmd::Previous),
            "--volume-up" => return Some(Cmd::VolumeUp),
            "--volume-down" => return Some(Cmd::VolumeDown),
            "--repeat" => return Some(Cmd::Repeat),
            "--shuffle" => return Some(Cmd::Shuffle),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            s if s.starts_with("--") => {
                eprintln!("music-tui: unknown flag '{s}'");
                eprintln!("Run 'music-tui --help' for usage.");
                std::process::exit(1);
            }
            _ => {}
        }
    }
    None
}

fn print_usage() {
    println!(
        "music-tui — terminal music player\n\
         \n\
         USAGE\n\
         \x20 music-tui            Start the TUI\n\
         \x20 music-tui --help     Show this help\n\
         \n\
         REMOTE CONTROL  (sends command to the running instance)\n\
         \x20 --play-pause         Toggle play/pause\n\
         \x20 --next               Skip to the next track\n\
         \x20 --previous           Go to the previous track\n\
         \x20 --volume-up          Raise the volume by 5%%\n\
         \x20 --volume-down        Lower the volume by 5%%\n\
         \x20 --repeat             Cycle the repeat mode\n\
         \x20 --shuffle            Toggle shuffle"
    );
}
