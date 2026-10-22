//! Session inventory, navigation, and the instance picker.
//!
//! A session supervisor holds an OS file lock for the lifetime of the login
//! shell. Old files are harmless: an unlocked record is not a live session.
//! Pane ownership is recorded separately and validated against Herdr's panes.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

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
}

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

pub fn session_key() -> io::Result<String> {
    let workspace = std::env::var("HERDR_WORKSPACE_ID").map_err(io_err)?;
    Ok(format!(
        "floax-{}",
        key(&format!("{}:{workspace}", state_dir()?.display()))
    ))
}

pub fn session_backend() -> io::Result<String> {
    let workspace = std::env::var("HERDR_WORKSPACE_ID").map_err(io_err)?;
    let record = live_record(&state_dir()?.join(format!("session-{}.json", key(&workspace))))?;
    Ok(record.map(|r| r.backend).unwrap_or_default())
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
    fs::write(
        dir.join(format!("pane-{}.json", key(id))),
        serde_json::to_vec(&json!({
            "pane_id": id,
            "terminal_id": text(pane, "terminal_id")?,
        }))?,
    )
}

pub fn register_view() -> io::Result<()> {
    let id = std::env::var("HERDR_PANE_ID").map_err(io_err)?;
    let result = api(&["pane", "get", &id])?;
    remember_pane(&result["pane"], &state_dir()?)
}

fn live_record(path: &Path) -> io::Result<Option<Record>> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match file.try_lock() {
        Ok(()) => return Ok(None), // Unlocked means the shell exited.
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(error)) => return Err(error),
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    // The supervisor may be between acquiring its lock and publishing JSON.
    Ok(serde_json::from_slice(&bytes).ok())
}

fn inventory(dir: &Path, workspaces: &[Value], panes: &[Value]) -> io::Result<Vec<Instance>> {
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
        let Some(record) = live_record(&path)? else {
            continue;
        };
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
    inventory(
        &state_dir()?,
        array(&workspaces, "workspaces")?,
        array(&panes, "panes")?,
    )
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

fn open_floating(target: &str, cwd: &str, dir: &Path) -> io::Result<()> {
    let result = api(&[
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
        &format!("HERDR_FLOAX_CWD={cwd}"),
        "--focus",
    ])?;
    let pane = &result["plugin_pane"]["pane"];
    remember_pane(pane, dir)?;
    focus_pane(text(pane, "pane_id")?)
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
        open_floating(
            text(target, "pane_id")?,
            target["cwd"].as_str().unwrap_or(""),
            &dir,
        )?;
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

fn navigate(instance: &Instance) -> io::Result<()> {
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
    if let Some(id) = fresh.pane_id {
        focus_pane(&id)?;
    } else {
        let dir = state_dir()?;
        let _guard = lock(&dir.join(format!("toggle-{}", key(&fresh.workspace_id))))?;
        // A toggle could have opened a view while we were refreshing.
        let result = api(&["pane", "list", "--workspace", &fresh.workspace_id])?;
        let panes = array(&result, "panes")?;
        if let Some(pane) = floating_panes(panes, &dir).first() {
            focus_pane(text(pane, "pane_id")?)?;
        } else {
            let target = panes
                .first()
                .ok_or_else(|| io_err("Workspace has no target pane"))?;
            open_floating(text(target, "pane_id")?, &fresh.cwd, &dir)?;
        }
    }
    Ok(())
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
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
    let rows: Vec<ListItem> = instances
        .iter()
        .map(|i| {
            let status = if !i.workspace_exists {
                "workspace closed"
            } else if i.pane_id.is_some() {
                "open"
            } else {
                "detached"
            };
            ListItem::new(vec![
                Line::from(format!(
                    "{} [{}] · {status} · {}",
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
    let mut state = ListState::default().with_selected((!instances.is_empty()).then_some(0));
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
                KeyCode::Down | KeyCode::Char('j') if !instances.is_empty() => {
                    state.select(Some(
                        (state.selected().unwrap_or(0) + 1).min(instances.len() - 1),
                    ));
                }
                KeyCode::Up | KeyCode::Char('k') if !instances.is_empty() => {
                    state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)));
                }
                KeyCode::Char('r') => match load_inventory() {
                    Ok(fresh) => {
                        instances = fresh;
                        state.select((!instances.is_empty()).then_some(0));
                        message.clear();
                    }
                    Err(error) => message = error.to_string(),
                },
                KeyCode::Enter => {
                    if let Some(i) = state.selected().and_then(|n| instances.get(n)) {
                        match navigate(i) {
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
        let dir = std::env::temp_dir().join(format!("floax-inventory-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut live = lock(&dir.join("session-live.json")).unwrap();
        live.write_all(br#"{"workspace_id":"w2","cwd":"/projects/api","backend":"tmux"}"#)
            .unwrap();
        fs::write(
            dir.join("session-dead.json"),
            br#"{"workspace_id":"w1","cwd":"/old","backend":"tmux"}"#,
        )
        .unwrap();
        let pane =
            json!({"pane_id":"w2:p3","terminal_id":"term3","workspace_id":"w2","label":"renamed"});
        remember_pane(&pane, &dir).unwrap();
        let workspaces = vec![json!({"workspace_id":"w2","label":"API"})];
        let open = inventory(&dir, &workspaces, std::slice::from_ref(&pane)).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].pane_id.as_deref(), Some("w2:p3"));
        assert_eq!(open[0].workspace_label, "API");
        assert_eq!(open[0].cwd, "/projects/api");
        let detached = inventory(&dir, &workspaces, &[]).unwrap();
        assert_eq!(detached.len(), 1);
        assert!(detached[0].pane_id.is_none());
        let mut reused_id = pane;
        reused_id["terminal_id"] = json!("different");
        assert!(inventory(&dir, &workspaces, &[reused_id]).unwrap()[0]
            .pane_id
            .is_none());
        drop(live);
        assert!(inventory(&dir, &workspaces, &[]).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}
