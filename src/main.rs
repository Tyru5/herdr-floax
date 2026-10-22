//! herdr-floax — a floating scratch shell for herdr, à la tmux-floax.
//!
//! Runs as one persistent herdr pane (opened as a zoomed split by the toggle
//! action) and draws a centered, sized box hosting a real shell PTY. herdr has
//! no sized floating-pane primitive that survives a keybinding — its
//! `overlay`/`zoomed` placements are transient views torn down when the
//! invoking action finishes — so the floating look is drawn here, in-process,
//! the same way herdr-file-viewer draws its help overlay. The box hosts a live
//! PTY rather than static text, which is what makes it a shell and not a modal.
//!
//! KNOWN LIMITATION: the backdrop around the box is a solid fill this app
//! paints, NOT the user's live panes dimmed behind it (tmux-floax shows the
//! real session through its popup). A plugin only controls its own pane's
//! canvas — herdr owns the other panes' PTYs and has no primitive for
//! compositing a persistent popup over them. See README.md "Limitations".
//!
//! Input is raw stdin passthrough: every byte herdr delivers to this pane goes
//! to the embedded shell verbatim (no lossy key-event translation), so vim,
//! REPLs, paste, and modifier chords all behave. herdr's prefix key never
//! reaches us — herdr intercepts it — so the toggle keybinding keeps working
//! while the shell is focused.
//!
//! The embedded program is scripts/floating-shell.sh (a login shell wrapped in
//! a per-workspace dtach/abduco/tmux session when available), so the session
//! survives the pane being closed on dismiss and re-attaches on reopen.

mod config;
mod instances;
mod ui;

use std::io::{Read, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;

/// Events that wake the render loop. Input never appears here — it flows
/// straight from stdin to the PTY on its own thread.
enum Ev {
    /// The embedded terminal produced output; redraw.
    Output,
    /// SIGWINCH: recompute the box and resize the PTY.
    Winch,
    /// The embedded program exited (or the PTY hit EOF); quit.
    Exit,
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
}

fn pty_size(inner: Rect) -> PtySize {
    PtySize {
        rows: inner.height,
        cols: inner.width,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn main() -> std::io::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--toggle") => return instances::toggle(),
        Some("--instances-open") => return instances::open_picker(),
        Some("--instances") => return instances::picker(),
        Some("--session-key") => {
            println!("{}", instances::session_key()?);
            return Ok(());
        }
        Some("--session-backend") => {
            println!("{}", instances::session_backend()?);
            return Ok(());
        }
        Some("--session-shell") => {
            let backend = std::env::args().nth(2).unwrap_or_else(|| "plain".into());
            return instances::session_shell(&backend);
        }
        Some(other) => return Err(io_err(format!("Unknown argument: {other}"))),
        None => {}
    }
    instances::register_view()?;
    let cfg = config::Config::load();

    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let inner = ui::box_inner(Rect::new(0, 0, cols, rows), &cfg);

    // Spawn the embedded shell sized to the box interior.
    let pty = native_pty_system();
    let pair = pty.openpty(pty_size(inner)).map_err(io_err)?;

    let root = std::env::var("HERDR_PLUGIN_ROOT").unwrap_or_else(|_| ".".into());
    let mut cmd = CommandBuilder::new("bash");
    cmd.arg(format!("{root}/scripts/floating-shell.sh"));
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    if let Ok(d) = std::env::var("HERDR_FLOAX_CWD") {
        if !d.is_empty() && std::path::Path::new(&d).is_dir() {
            cmd.cwd(d);
        }
    }
    let mut child = pair.slave.spawn_command(cmd).map_err(io_err)?;
    drop(pair.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new(inner.height, inner.width, 0)));
    let (tx, rx) = mpsc::channel::<Ev>();

    // PTY output → vt100 parser → redraw.
    {
        let parser = Arc::clone(&parser);
        let tx = tx.clone();
        let mut reader = pair.master.try_clone_reader().map_err(io_err)?;
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(Ev::Exit);
                        break;
                    }
                    Ok(n) => {
                        parser.lock().unwrap().process(&buf[..n]);
                        if tx.send(Ev::Output).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    // stdin → PTY, raw byte passthrough.
    {
        let mut writer = pair.master.take_writer().map_err(io_err)?;
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 2048];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                }
            }
            // portable-pty's writer destructor sends newline + VEOF. On view
            // dismissal that would reach the detached shell and exit it.
            // Keep this descriptor until process exit instead of injecting EOF.
            std::mem::forget(writer);
        });
    }

    // Embedded program exit → quit.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = tx.send(Ev::Exit);
        });
    }

    // SIGWINCH → re-layout.
    {
        let tx = tx.clone();
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        std::thread::spawn(move || {
            for _ in signals.forever() {
                if tx.send(Ev::Winch).is_err() {
                    break;
                }
            }
        });
    }

    // Terminal up. Restore on panic too, so a bug never wedges the pane raw.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let master = pair.master;
    loop {
        {
            let parser = parser.lock().unwrap();
            terminal.draw(|f| ui::draw(f, &cfg, parser.screen()))?;
        }
        let Ok(first) = rx.recv() else { break };
        let mut exit = matches!(first, Ev::Exit);
        let mut winch = matches!(first, Ev::Winch);
        // Coalesce bursts: one redraw per batch of PTY output.
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Ev::Exit => exit = true,
                Ev::Winch => winch = true,
                Ev::Output => {}
            }
        }
        if exit {
            break;
        }
        if winch {
            let (c, r) = ratatui::crossterm::terminal::size().unwrap_or((cols, rows));
            let inner = ui::box_inner(Rect::new(0, 0, c, r), &cfg);
            let _ = master.resize(pty_size(inner));
            parser.lock().unwrap().set_size(inner.height, inner.width);
        }
    }

    restore_terminal();
    Ok(())
}
