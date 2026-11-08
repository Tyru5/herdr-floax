//! Session inventory, navigation, and the instance picker.
//!
//! A session supervisor holds an OS file lock for the lifetime of the login
//! shell. Old files are harmless: an unlocked record is not a live session.
//! Pane ownership is recorded separately and validated against Herdr's panes.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::io_err;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    workspace_id: String,
    cwd: String,
    backend: String,
}

#[derive(Clone, Debug)]
struct Instance {
    workspace_id: String,
    workspace_label: String,
    pane_id: Option<String>,
    cwd: String,
    backend: String,
    workspace_exists: bool,
    /// tmux session hosting this root's shell.
    session: String,
    /// Every tmux window in this root's family (its own session plus sessions
    /// created from inside it), when there is more than one. Empty otherwise.
    windows: Vec<Window>,
}

/// One tmux window on the Floax tmux server.
#[derive(Clone, Debug, PartialEq)]
struct Window {
    session_id: String,
    session_name: String,
    window_id: String,
    index: u32,
    name: String,
    cwd: String,
    /// The window a client currently shows.
    active: bool,
    /// `HERDR_FLOAX_ROOT` of the window's session: the root session it was
    /// created from (see floating-shell.sh).
    root: Option<String>,
}

const TMUX_SOCKET: &str = "herdr-floax";
const ROOT_VAR: &str = "HERDR_FLOAX_ROOT";
const WINDOW_FORMAT: &str = "#{session_id}\t#{session_name}\t#{window_id}\t#{window_index}\t#{window_active}\t#{session_attached}\t#{pane_current_path}\t#{window_name}";

// Fixed FNV-1a encoding, rather than a process- or Rust-version-dependent hash.
fn key(text: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn state_dir() -> io::Result<PathBuf> {
    let socket = std::env::var("HERDR_SOCKET_PATH").map_err(io_err)?;
    let metadata = fs::metadata(&socket)?;
    // A new server at the same socket path must not inherit old workspace IDs.
    let scope = key(&format!("{socket}:{}:{}", metadata.dev(), metadata.ino()));
    let root = std::env::var("HERDR_PLUGIN_STATE_DIR").map_err(io_err)?;
    let dir = PathBuf::from(root).join("instances").join(scope);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn session_name(dir: &Path, workspace: &str) -> String {
    format!("floax-{}", key(&format!("{}:{workspace}", dir.display())))
}

pub fn session_key() -> io::Result<String> {
    let workspace = std::env::var("HERDR_WORKSPACE_ID").map_err(io_err)?;
    Ok(session_name(&state_dir()?, &workspace))
}

pub fn session_backend() -> io::Result<String> {
    let workspace = std::env::var("HERDR_WORKSPACE_ID").map_err(io_err)?;
    let dir = state_dir()?;
    let backend = match read_record(&dir.join(format!("session-{}.json", key(&workspace))))? {
        Some((record, true)) => record.backend,
        // The first shell exited, but other tmux windows keep the session up.
        Some((record, false))
            if record.backend == "tmux"
                && tmux(&[
                    "has-session",
                    "-t",
                    &format!("={}", session_name(&dir, &workspace)),
                ])
                .is_some() =>
        {
            record.backend
        }
        _ => String::new(),
    };
    Ok(backend)
}

/// Runs a command on the Floax tmux server. `None` when tmux is missing,
/// the server is down, or the command fails: tmux state is best effort.
fn tmux(args: &[&str]) -> Option<String> {
    let out = Command::new("tmux")
        .args(["-L", TMUX_SOCKET])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_windows(text: &str) -> Vec<Window> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.splitn(8, '\t');
            let session_id = f.next()?.into();
            let session_name = f.next()?.into();
            let window_id = f.next()?.into();
            let index = f.next()?.parse().ok()?;
            let active = f.next()? == "1";
            let attached = f.next()?.parse::<u32>().ok()? > 0;
            Some(Window {
                session_id,
                session_name,
                window_id,
                index,
                active: active && attached,
                cwd: f.next()?.into(),
                name: f.next()?.into(),
                root: None,
            })
        })
        .collect()
}

fn tmux_windows() -> Vec<Window> {
    let Some(text) = tmux(&["list-windows", "-a", "-F", WINDOW_FORMAT]) else {
        return Vec::new();
    };
    let mut windows = parse_windows(&text);
    let mut roots: Vec<(String, Option<String>)> = Vec::new();
    for window in &mut windows {
        // Root sessions are identified by name; only look up the others.
        if window.session_name.starts_with("floax-") {
            continue;
        }
        if let Some((_, root)) = roots.iter().find(|(id, _)| *id == window.session_id) {
            window.root = root.clone();
            continue;
        }
        let root =
            tmux(&["show-environment", "-t", &window.session_id, ROOT_VAR]).and_then(|out| {
                out.trim()
                    .strip_prefix(&format!("{ROOT_VAR}="))
                    .map(String::from)
            });
        roots.push((window.session_id.clone(), root.clone()));
        window.root = root;
    }
    windows
}

/// The root session's windows first, then sessions created from it.
fn family(windows: &[Window], root: &str) -> Vec<Window> {
    let mut family: Vec<Window> = windows
        .iter()
        .filter(|w| w.session_name == root || w.root.as_deref() == Some(root))
        .cloned()
        .collect();
    family.sort_by(|a, b| {
        (a.session_name != root, &a.session_name, a.index).cmp(&(
            b.session_name != root,
            &b.session_name,
            b.index,
        ))
    });
    family
}

fn lock(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.try_lock().map_err(io_err)?;
    Ok(file)
}

/// Runs *inside* dtach/abduco/tmux, so its lock survives view dismissal.
pub fn session_shell(backend: &str) -> io::Result<()> {
    let workspace_id = std::env::var("HERDR_WORKSPACE_ID").map_err(io_err)?;
    let record = Record {
        workspace_id: workspace_id.clone(),
        cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
        backend: backend.into(),
    };
    let mut file = lock(&state_dir()?.join(format!("session-{}.json", key(&workspace_id))))?;
    file.set_len(0)?;
    file.write_all(serde_json::to_string(&record)?.as_bytes())?;
    file.flush()?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let status = Command::new(shell).arg("-l").status()?;
    // Release before process::exit; stale files never count as running.
    drop(file);
    std::process::exit(status.code().unwrap_or(1));
}

fn api(args: &[&str]) -> io::Result<Value> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let out = Command::new(bin).args(args).output()?;
    let value: Value = serde_json::from_slice(&out.stdout).map_err(|error| {
        io_err(format!(
            "Herdr {}: {error}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        ))
    })?;
    if !out.status.success() || value.get("error").is_some() {
        return Err(io_err(format!("Herdr {}: {}", args.join(" "), value)));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| io_err("Herdr response has no result"))
}

fn array<'a>(result: &'a Value, field: &str) -> io::Result<&'a Vec<Value>> {
    result[field]
        .as_array()
        .ok_or_else(|| io_err(format!("Herdr response has no {field} list")))
}

fn text<'a>(value: &'a Value, field: &str) -> io::Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| io_err(format!("Herdr response has no {field}")))
}

fn floating_panes(panes: &[Value], dir: &Path) -> Vec<Value> {
    panes
        .iter()
        .filter(|pane| {
            pane["pane_id"].as_str().is_some_and(|id| {
                fs::read(dir.join(format!("pane-{}.json", key(id))))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .is_some_and(|saved| {
                        saved["pane_id"] == pane["pane_id"]
                            && saved["terminal_id"] == pane["terminal_id"]
                    })
            })
        })
        .cloned()
        .collect()
}

fn remember_pane(pane: &Value, dir: &Path) -> io::Result<()> {
    let id = text(pane, "pane_id")?;
    // Store stable identity only. Labels, cwd, and focus can all change.
    let mut pending = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(
        &mut pending,
        &json!({
            "pane_id": id,
            "terminal_id": text(pane, "terminal_id")?,
        }),
    )?;
    // Concurrent registration and inventory reads see either complete version.
    pending
        .persist(dir.join(format!("pane-{}.json", key(id))))
        .map_err(|error| error.error)?;
    Ok(())
}

pub fn register_view() -> io::Result<()> {
    let id = std::env::var("HERDR_PANE_ID").map_err(io_err)?;
    let result = api(&["pane", "get", &id])?;
    remember_pane(&result["pane"], &state_dir()?)
}

/// A session record and whether its supervisor still holds the lock.
fn read_record(path: &Path) -> io::Result<Option<(Record, bool)>> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let locked = match file.try_lock() {
        Ok(()) => false, // Unlocked means the first shell exited.
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(error)) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // The supervisor may be between acquiring its lock and publishing JSON.
    Ok(serde_json::from_slice(&bytes).ok().map(|r| (r, locked)))
}

fn inventory(
    dir: &Path,
    workspaces: &[Value],
    panes: &[Value],
    windows: &[Window],
) -> io::Result<Vec<Instance>> {
    let views = floating_panes(panes, dir);
    let mut instances = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if !path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("session-"))
        {
            continue;
        }
        let Some((record, locked)) = read_record(&path)? else {
            continue;
        };
        let session = session_name(dir, &record.workspace_id);
        let family = if record.backend == "tmux" {
            family(windows, &session)
        } else {
            Vec::new()
        };
        // A tmux session outlives its first shell while other windows remain.
        if !locked && !family.iter().any(|w| w.session_name == session) {
            continue;
        }
        let workspace = workspaces
            .iter()
            .find(|w| w["workspace_id"] == record.workspace_id);
        let pane = views
            .iter()
            .find(|p| p["workspace_id"] == record.workspace_id);
        instances.push(Instance {
            workspace_label: workspace
                .and_then(|w| w["label"].as_str())
                .filter(|label| !label.is_empty())
                .unwrap_or(&record.workspace_id)
                .into(),
            pane_id: pane.and_then(|p| p["pane_id"].as_str()).map(String::from),
            workspace_exists: workspace.is_some(),
            workspace_id: record.workspace_id,
            cwd: record.cwd,
            backend: record.backend,
            session,
            windows: if family.len() > 1 { family } else { Vec::new() },
        });
    }
    instances.sort_by(|a, b| {
        (&a.workspace_label, &a.workspace_id).cmp(&(&b.workspace_label, &b.workspace_id))
    });
    Ok(instances)
}

fn load_inventory() -> io::Result<Vec<Instance>> {
    let workspaces = api(&["workspace", "list"])?;
    let panes = api(&["pane", "list"])?;
    let dir = state_dir()?;
    // Only tmux sessions have windows; skip the tmux calls otherwise.
    let windows = if tmux_records(&dir) {
        tmux_windows()
    } else {
        Vec::new()
    };
    inventory(
        &dir,
        array(&workspaces, "workspaces")?,
        array(&panes, "panes")?,
        &windows,
    )
}

fn tmux_records(dir: &Path) -> bool {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.file_name().to_string_lossy().starts_with("session-")
                && fs::read(entry.path())
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
                    .is_some_and(|r| r.backend == "tmux")
        })
}

fn focus_pane(id: &str) -> io::Result<()> {
    let result = api(&["plugin", "pane", "focus", id])?;
    // Herdr 0.9.5 plugin focus changes app state but not the attached clients'
    // visible workspace/tab. Explicit tab focus propagates that navigation.
    api(&[
        "tab",
        "focus",
        text(&result["plugin_pane"]["pane"], "tab_id")?,
    ])?;
    api(&["pane", "zoom", id, "--on"])?;
    Ok(())
}

/// `attach` names a tmux session to show instead of the root session.
fn open_floating(target: &str, cwd: &str, dir: &Path, attach: Option<&str>) -> io::Result<String> {
    let cwd_env = format!("HERDR_FLOAX_CWD={cwd}");
    let attach_env = attach.map(|session| format!("HERDR_FLOAX_ATTACH={session}"));
    let mut args = vec![
        "plugin",
        "pane",
        "open",
        "--plugin",
        "herdr-floax",
        "--entrypoint",
        "floating",
        "--placement",
        "split",
        "--direction",
        "right",
        "--target-pane",
        target,
        "--env",
        &cwd_env,
        "--focus",
    ];
    if let Some(env) = &attach_env {
        args.extend(["--env", env]);
    }
    let result = api(&args)?;
    let pane = &result["plugin_pane"]["pane"];
    remember_pane(pane, dir)?;
    Ok(text(pane, "pane_id")?.into())
}

pub fn toggle() -> io::Result<()> {
    let dir = state_dir()?;
    let current = api(&["pane", "current"])?;
    let workspace = std::env::var("HERDR_WORKSPACE_ID")
        .unwrap_or(text(&current["pane"], "workspace_id")?.into());
    // Serialize rapid toggles so two actions cannot both decide to create.
    let _guard = lock(&dir.join(format!("toggle-{}", key(&workspace))))?;
    let result = api(&["pane", "list", "--workspace", &workspace])?;
    let panes = array(&result, "panes")?;
    if let Some(pane) = floating_panes(panes, &dir).first() {
        let id = text(pane, "pane_id")?;
        if pane["focused"] == true {
            api(&["plugin", "pane", "close", id])?;
        } else {
            focus_pane(id)?;
        }
    } else {
        let target = panes
            .iter()
            .find(|p| p["focused"] == true)
            .or_else(|| panes.first())
            .ok_or_else(|| io_err("Workspace has no target pane"))?;
        let id = open_floating(
            text(target, "pane_id")?,
            target["cwd"].as_str().unwrap_or(""),
            &dir,
            None,
        )?;
        focus_pane(&id)?;
    }
    Ok(())
}

pub fn open_picker() -> io::Result<()> {
    let result = api(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "herdr-floax",
        "--entrypoint",
        "instances",
        "--placement",
        "split",
        "--direction",
        "right",
        "--focus",
    ])?;
    focus_pane(text(&result["plugin_pane"]["pane"], "pane_id")?)
}

/// Shows `window` in every tmux client attached to this root's family. A
/// workspace has at most one view, so in practice that is one client.
fn show_window(fresh: &Instance, window: &Window) -> io::Result<()> {
    tmux(&["select-window", "-t", &window.window_id])
        .ok_or_else(|| io_err("Window has closed. Press r to refresh."))?;
    let clients = tmux(&["list-clients", "-F", "#{client_name}\t#{session_name}"]);
    for line in clients.as_deref().unwrap_or_default().lines() {
        let Some((client, session)) = line.split_once('\t') else {
            continue;
        };
        let in_family =
            session == fresh.session || fresh.windows.iter().any(|w| w.session_name == session);
        if in_family && session != window.session_name {
            tmux(&["switch-client", "-c", client, "-t", &window.session_id]);
        }
    }
    Ok(())
}

fn navigate(instance: &Instance, window: Option<&Window>) -> io::Result<()> {
    // Refresh before acting: the selected session or its view may have exited.
    let fresh = load_inventory()?
        .into_iter()
        .find(|i| i.workspace_id == instance.workspace_id)
        .ok_or_else(|| io_err("Session has exited. Press r to refresh."))?;
    if !fresh.workspace_exists {
        return Err(io_err(
            "Workspace was closed. This session cannot be reattached here.",
        ));
    }
    let window = match window {
        Some(w) => Some(
            fresh
                .windows
                .iter()
                .find(|f| f.window_id == w.window_id)
                .ok_or_else(|| io_err("Window has closed. Press r to refresh."))?,
        ),
        None => None,
    };
    if let Some(window) = window {
        show_window(&fresh, window)?;
    }
    let target = if let Some(id) = fresh.pane_id.clone() {
        id
    } else {
        let dir = state_dir()?;
        let _guard = lock(&dir.join(format!("toggle-{}", key(&fresh.workspace_id))))?;
        // A toggle could have opened a view while we were refreshing.
        let result = api(&["pane", "list", "--workspace", &fresh.workspace_id])?;
        let panes = array(&result, "panes")?;
        if let Some(pane) = floating_panes(panes, &dir).first() {
            text(pane, "pane_id")?.into()
        } else {
            let target = panes
                .first()
                .ok_or_else(|| io_err("Workspace has no target pane"))?;
            // A new view attaches the root session unless told otherwise.
            let attach = window
                .filter(|w| w.session_name != fresh.session)
                .map(|w| w.session_id.as_str());
            open_floating(text(target, "pane_id")?, &fresh.cwd, &dir, attach)?
        }
    };
    let mut command = Command::new(std::env::current_exe()?);
    command.args(["--finish-navigation", &target]);
    // Herdr kills the picker's entire OS session on close. The helper must
    // survive that close to zoom AFTER split removal clears the tab's zoom.
    // SAFETY: setsid is async-signal-safe; no allocations or locks after fork.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // Keep the picker alive until the helper closes it. Pipes isolate helper
    // I/O from the dying PTY and let pre-close failures remain visible here.
    let output = command.output()?;
    if !output.status.success() {
        return Err(io_err(String::from_utf8_lossy(&output.stderr)));
    }
    Ok(())
}

pub fn finish_navigation(target: &str) -> io::Result<()> {
    let picker = std::env::var("HERDR_PANE_ID").map_err(io_err)?;
    // A successful close response is the layout-removal barrier. No sleep or
    // process-exit polling: those can race Herdr's asynchronous pane cleanup.
    api(&["plugin", "pane", "close", &picker])?;
    focus_pane(target)
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Picker rows: each root, followed by its nested windows.
fn rows(instances: &[Instance]) -> Vec<(usize, Option<usize>)> {
    instances
        .iter()
        .enumerate()
        .flat_map(|(n, i)| {
            std::iter::once((n, None)).chain((0..i.windows.len()).map(move |c| (n, Some(c))))
        })
        .collect()
}

fn window_line(instance: &Instance, child: usize) -> Line<'static> {
    let w = &instance.windows[child];
    let branch = if child + 1 == instance.windows.len() {
        "└─"
    } else {
        "├─"
    };
    // Root-session windows show only their index; other sessions are named.
    let session = if w.session_name == instance.session {
        String::new()
    } else {
        format!("{}:", clean(&w.session_name))
    };
    let active = if w.active { " · active" } else { "" };
    Line::from(vec![
        Span::styled(format!("  {branch} "), Style::default().fg(Color::DarkGray)),
        Span::raw(format!("{session}{} {}", w.index, clean(&w.name))),
        Span::styled(
            format!(" · {}{active}", clean(&w.cwd)),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

fn draw_picker(frame: &mut Frame, instances: &[Instance], state: &mut ListState, message: &str) {
    let area = frame.area();
    let title = if message.is_empty() {
        format!(" Floax · {} running sessions ", instances.len())
    } else {
        " Floax · refresh needed ".into()
    };
    let block = Block::bordered()
        .title(title)
        .title_bottom(" ↑↓/jk select · Enter jump/reattach · r refresh · Esc close ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).areas(inner);
    let rows: Vec<ListItem> = rows(instances)
        .into_iter()
        .map(|(n, child)| {
            let i = &instances[n];
            if let Some(c) = child {
                return ListItem::new(window_line(i, c));
            }
            let status = if !i.workspace_exists {
                "workspace closed"
            } else if i.pane_id.is_some() {
                "open"
            } else {
                "detached"
            };
            let windows = if i.windows.is_empty() {
                String::new()
            } else {
                format!(" · {} windows", i.windows.len())
            };
            ListItem::new(vec![
                Line::from(format!(
                    "{} [{}] · {status} · {}{windows}",
                    clean(&i.workspace_label),
                    clean(&i.workspace_id),
                    clean(&i.backend)
                )),
                Line::from(format!("  Launch directory: {}", clean(&i.cwd))),
            ])
        })
        .collect();
    frame.render_stateful_widget(
        List::new(rows)
            .highlight_symbol("> ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        list,
        state,
    );
    let message = if message.is_empty() && instances.is_empty() {
        "No running Floax sessions. Esc closes this list."
    } else {
        message
    };
    frame.render_widget(
        Paragraph::new(clean(message)).wrap(ratatui::widgets::Wrap { trim: false }),
        status,
    );
}

pub fn picker() -> io::Result<()> {
    // Keep API failures visible, rather than turning them into an empty count.
    let initial = load_inventory();
    let mut message = initial
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    let mut instances = initial.unwrap_or_default();
    let mut entries = rows(&instances);
    let mut state = ListState::default().with_selected((!entries.is_empty()).then_some(0));
    ratatui::crossterm::terminal::enable_raw_mode()?;
    ratatui::crossterm::execute!(
        io::stdout(),
        ratatui::crossterm::terminal::EnterAlternateScreen
    )?;
    let result = (|| -> io::Result<()> {
        let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        loop {
            terminal.draw(|f| draw_picker(f, &instances, &mut state, &message))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Down | KeyCode::Char('j') if !entries.is_empty() => {
                    state.select(Some(
                        (state.selected().unwrap_or(0) + 1).min(entries.len() - 1),
                    ));
                }
                KeyCode::Up | KeyCode::Char('k') if !entries.is_empty() => {
                    state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)));
                }
                KeyCode::Char('r') => match load_inventory() {
                    Ok(fresh) => {
                        instances = fresh;
                        entries = rows(&instances);
                        state.select((!entries.is_empty()).then_some(0));
                        message.clear();
                    }
                    Err(error) => message = error.to_string(),
                },
                KeyCode::Enter => {
                    if let Some(&(n, child)) = state.selected().and_then(|n| entries.get(n)) {
                        let i = &instances[n];
                        match navigate(i, child.map(|c| &i.windows[c])) {
                            Ok(()) => break,
                            Err(error) => message = error.to_string(),
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })();
    crate::restore_terminal();
    // Exiting the process closes this temporary split and restores its layout.
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_counts_live_detached_shells_and_survives_renames() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        let mut live = lock(&dir.join("session-live.json")).unwrap();
        live.write_all(br#"{"workspace_id":"w2","cwd":"/projects/api","backend":"tmux"}"#)
            .unwrap();
        fs::write(
            dir.join("session-dead.json"),
            br#"{"workspace_id":"w1","cwd":"/old","backend":"tmux"}"#,
        )
        .unwrap();
        let pane = json!({"pane_id":"w2:p3","terminal_id":"term3".repeat(4096),"workspace_id":"w2","label":"renamed"});
        remember_pane(&pane, &dir).unwrap();
        let workspaces = vec![json!({"workspace_id":"w2","label":"API"})];
        let open = inventory(&dir, &workspaces, std::slice::from_ref(&pane), &[]).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].pane_id.as_deref(), Some("w2:p3"));
        assert_eq!(open[0].workspace_label, "API");
        assert_eq!(open[0].cwd, "/projects/api");
        // Registration and the opening action can publish the same view
        // concurrently. Readers must always see a complete pane identity.
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    for _ in 0..200 {
                        remember_pane(&pane, &dir).unwrap();
                    }
                });
            }
            for _ in 0..400 {
                let current =
                    inventory(&dir, &workspaces, std::slice::from_ref(&pane), &[]).unwrap();
                assert_eq!(current[0].pane_id.as_deref(), Some("w2:p3"));
            }
        });
        let detached = inventory(&dir, &workspaces, &[], &[]).unwrap();
        assert_eq!(detached.len(), 1);
        assert!(detached[0].pane_id.is_none());
        let mut reused_id = pane;
        reused_id["terminal_id"] = json!("different");
        assert!(inventory(&dir, &workspaces, &[reused_id], &[]).unwrap()[0]
            .pane_id
            .is_none());
        drop(live);
        assert!(inventory(&dir, &workspaces, &[], &[]).unwrap().is_empty());
    }

    #[test]
    fn tmux_windows_and_child_sessions_nest_under_their_root() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_owned();
        let root = session_name(&dir, "w1");
        let other = session_name(&dir, "w2");
        let mut live = lock(&dir.join("session-a.json")).unwrap();
        live.write_all(br#"{"workspace_id":"w1","cwd":"/a","backend":"tmux"}"#)
            .unwrap();
        let mut single = lock(&dir.join("session-b.json")).unwrap();
        single
            .write_all(br#"{"workspace_id":"w2","cwd":"/b","backend":"tmux"}"#)
            .unwrap();
        let text = format!(
            "$1\tscratch\t@5\t0\t1\t0\t/s\tzsh\n\
             $0\t{root}\t@1\t1\t1\t1\t/a/src\tvim\n\
             $0\t{root}\t@0\t0\t0\t1\t/a\tzsh\n\
             $2\t{other}\t@7\t0\t1\t1\t/b\tzsh\n\
             $3\tstray\t@8\t0\t1\t0\t/x\tzsh\n\
             malformed line\n"
        );
        let mut windows = parse_windows(&text);
        assert_eq!(windows.len(), 5);
        assert!(
            !windows[0].active,
            "detached sessions show no active window"
        );
        windows[0].root = Some(root.clone());
        windows[4].root = Some("floax-elsewhere".into());
        let found = inventory(&dir, &[], &[], &windows).unwrap();
        let a = found.iter().find(|i| i.workspace_id == "w1").unwrap();
        let ids: Vec<_> = a.windows.iter().map(|w| w.window_id.as_str()).collect();
        assert_eq!(ids, ["@0", "@1", "@5"], "root windows first, then children");
        assert!(a.windows[1].active);
        let b = found.iter().find(|i| i.workspace_id == "w2").unwrap();
        assert!(b.windows.is_empty(), "a lone window is not nested");
        let flat = rows(&found);
        assert_eq!(flat.len(), found.len() + 3);

        // The first shell exited, but tmux keeps the session for other windows.
        drop(live);
        let found = inventory(&dir, &[], &[], &windows).unwrap();
        assert!(found.iter().any(|i| i.workspace_id == "w1"));
        let gone: Vec<_> = windows
            .iter()
            .filter(|w| w.session_name != root)
            .cloned()
            .collect();
        let found = inventory(&dir, &[], &[], &gone).unwrap();
        assert!(!found.iter().any(|i| i.workspace_id == "w1"));
        drop(single);
    }
}
