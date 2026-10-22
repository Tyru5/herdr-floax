use std::{fs, process::Command};

#[test]
fn upgrade_preserves_custom_toggle_and_rejects_picker_collision() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let cfg = dir.join("config.toml");
    let original = "# herdr-floax:keybind\n[[keys.command]]\nkey = \"prefix+i\"\ntype = \"plugin_action\"\ncommand = \"herdr-floax.toggle\"\n";
    fs::write(&cfg, original).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_herdr-floax"))
            .arg("--install-keybindings")
            .args(args)
            .env("HERDR_CONFIG_DIR", dir)
            .env_remove("HERDR_FLOAX_KEY")
            .env_remove("HERDR_FLOAX_INSTANCES_KEY")
            .output()
            .unwrap()
    };
    assert!(!run(&[]).status.success());
    assert_eq!(fs::read_to_string(&cfg).unwrap(), original);
    assert!(run(&["prefix+f", "prefix+G"]).status.success());
    let upgraded = fs::read_to_string(&cfg).unwrap();
    assert!(upgraded.starts_with(original));
    assert!(upgraded.contains("key = \"prefix+G\""));
    assert_eq!(upgraded.matches("herdr-floax.instances").count(), 1);
    assert!(run(&[]).status.success());
    assert_eq!(fs::read_to_string(&cfg).unwrap(), upgraded);
    let literal = original.replace("\"prefix+i\"", "'prefix+i'");
    fs::write(&cfg, &literal).unwrap();
    assert!(!run(&[]).status.success());
    assert_eq!(fs::read_to_string(&cfg).unwrap(), literal);
    for edited in [
        original.replace("# herdr-floax:keybind", "  # herdr-floax:keybind  "),
        original.replace("prefix+i", "prefix+\\u0069"),
        original.replace("# herdr-floax:keybind\n", ""),
        "[[keys.command]]\ncommand = 'herdr-floax.toggle'\nkey = 'prefix+i'\n".into(),
    ] {
        fs::write(&cfg, &edited).unwrap();
        assert!(!run(&[]).status.success());
        assert_eq!(fs::read_to_string(&cfg).unwrap(), edited);
    }
    let file_viewer = "[[keys.command]]\nkey = \"prefix+F\"\ntype = \"plugin_action\"\ncommand = \"herdr-file-viewer.open\"\n";
    fs::write(&cfg, file_viewer).unwrap();
    assert!(run(&[]).status.success());
    let installed = fs::read_to_string(&cfg).unwrap();
    assert!(installed.starts_with(file_viewer));
    let parsed: toml::Value = installed.parse().unwrap();
    assert!(parsed["keys"]["command"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["command"].as_str() == Some("herdr-floax.instances")
            && b["key"].as_str() == Some("prefix+i")));
    assert_eq!(installed.matches("prefix+F").count(), 1);
    let manual = "[[keys.command]]\ncommand = 'herdr-floax.toggle'\nkey = 'prefix+x'\n\n[[keys.command]]\nkey = \"prefix+\\u006a\"\ncommand = 'herdr-floax.instances'\n";
    fs::write(&cfg, manual).unwrap();
    assert!(run(&[]).status.success());
    assert_eq!(fs::read_to_string(&cfg).unwrap(), manual);
    for unsupported in [
        "[keys]\ncommand = [{key = 'prefix+x', command = 'herdr-floax.toggle'}]\n",
        "[[keys.command]]\nkey = 'prefix+i'\ncommand = 'another-plugin.open'\n",
    ] {
        fs::write(&cfg, unsupported).unwrap();
        assert!(!run(&[]).status.success());
        assert_eq!(fs::read_to_string(&cfg).unwrap(), unsupported);
    }
    fs::remove_file(&cfg).unwrap();
    assert!(!run(&["prefix+i", "prefix+i"]).status.success());
    assert!(!cfg.exists());
    assert!(run(&[]).status.success());
    let fresh: toml::Value = fs::read_to_string(&cfg).unwrap().parse().unwrap();
    assert_eq!(fresh["keys"]["command"].as_array().unwrap().len(), 2);
}
