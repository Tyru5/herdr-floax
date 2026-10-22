use std::{fs, process::Command};

#[test]
fn upgrade_preserves_custom_toggle_and_rejects_picker_collision() {
    let dir = std::env::temp_dir().join(format!("floax-keys-test-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config.toml");
    let original = "# herdr-floax:keybind\n[[keys.command]]\nkey = \"prefix+i\"\ntype = \"plugin_action\"\ncommand = \"herdr-floax.toggle\"\n";
    fs::write(&cfg, original).unwrap();
    let run = |args: &[&str]| {
        Command::new("bash")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/install-keybinding.sh"
            ))
            .args(args)
            .env("HERDR_CONFIG_DIR", &dir)
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
    assert!(installed.contains(
        "key = \"prefix+i\"\ntype = \"plugin_action\"\ncommand = \"herdr-floax.instances\""
    ));
    assert_eq!(installed.matches("prefix+F").count(), 1);
    fs::remove_dir_all(dir).unwrap();
}
