//! Install missing Floax bindings without rewriting the user's TOML.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::io_err;

pub fn install() -> io::Result<()> {
    let mut args = std::env::args().skip(2);
    let toggle = args
        .next()
        .or_else(|| std::env::var("HERDR_FLOAX_KEY").ok())
        .unwrap_or_else(|| "prefix+f".into());
    let picker = args
        .next()
        .or_else(|| std::env::var("HERDR_FLOAX_INSTANCES_KEY").ok())
        .unwrap_or_else(|| "prefix+i".into());
    let dir = match std::env::var_os("HERDR_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| io_err("HOME is unset"))?)
            .join(".config/herdr"),
    };
    let cfg = dir.join("config.toml");
    let original = match fs::read_to_string(&cfg) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let document: toml::Table = toml::from_str(&original).map_err(io_err)?;
    let bindings = document
        .get("keys")
        .and_then(|keys| keys.get("command"))
        .map(|commands| {
            commands
                .as_array()
                .ok_or_else(|| io_err("keys.command must be an array"))
        })
        .transpose()?
        .map(Vec::as_slice)
        .unwrap_or_default();
    let requested = [
        (
            "herdr-floax.toggle",
            toggle,
            "Toggle floating pane",
            "keybind",
        ),
        (
            "herdr-floax.instances",
            picker,
            "List and navigate Floax sessions",
            "instances-keybind",
        ),
    ];
    let missing: Vec<_> = requested
        .iter()
        .filter(|(command, ..)| {
            !bindings
                .iter()
                .any(|b| b.get("command").and_then(toml::Value::as_str) == Some(command))
        })
        .collect();
    // Validate every addition before writing anything. Existing bindings can
    // appear in either key/command order, with any TOML quote or escape style.
    for (index, (command, key, ..)) in missing.iter().enumerate() {
        if key.trim().is_empty() {
            return Err(io_err("Keybindings cannot be empty"));
        }
        if bindings
            .iter()
            .any(|b| b.get("key").and_then(toml::Value::as_str) == Some(key))
            || missing[..index].iter().any(|(_, other, ..)| other == key)
        {
            return Err(io_err(format!(
                "Cannot bind {command} to {key}: that key is already assigned. Choose another key or edit {}.",
                cfg.display()
            )));
        }
    }
    let mut additions = String::new();
    for (command, key, description, marker) in missing {
        let mut binding = toml::Table::new();
        for (name, value) in [
            ("key", key.as_str()),
            ("type", "plugin_action"),
            ("command", *command),
            ("description", *description),
        ] {
            binding.insert(name.into(), toml::Value::String(value.into()));
        }
        additions.push_str(&format!(
            "\n# herdr-floax:{marker}\n[[keys.command]]\n{}",
            toml::to_string(&binding).map_err(io_err)?
        ));
    }
    if !additions.is_empty() {
        // Some valid TOML layouts, such as inline arrays, cannot be extended
        // with array-table blocks. Leave those configs untouched on failure.
        toml::from_str::<toml::Table>(&format!("{original}{additions}")).map_err(io_err)?;
        fs::create_dir_all(&dir)?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&cfg)?
            .write_all(additions.as_bytes())?;
    }
    println!("Floax keybindings installed or already present in {}. Reload Herdr config to apply changes.", cfg.display());
    Ok(())
}
