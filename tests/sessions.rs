use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(dir: &Path, socket: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-floax"));
    command
        .env("HERDR_PLUGIN_STATE_DIR", dir.join("state"))
        .env("HERDR_SOCKET_PATH", dir.join(socket))
        .env("HERDR_WORKSPACE_ID", "w1")
        .env("SHELL", dir.join("shell"));
    command
}

fn output(dir: &Path, socket: &str, argument: &str) -> String {
    let output = command(dir, socket).arg(argument).output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[test]
fn live_session_backend_and_names_are_scoped_to_the_server() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_owned();
    // Only socket metadata is used for namespacing. No Herdr server needed.
    fs::write(dir.join("server-a"), "").unwrap();
    fs::write(dir.join("server-b"), "").unwrap();
    fs::write(dir.join("shell"), "#!/bin/sh\nread -r line\n").unwrap();
    fs::set_permissions(dir.join("shell"), fs::Permissions::from_mode(0o700)).unwrap();

    let a = output(&dir, "server-a", "--session-key");
    assert_eq!(a, output(&dir, "server-a", "--session-key"));
    assert_ne!(a, output(&dir, "server-b", "--session-key"));
    assert_eq!(output(&dir, "server-a", "--session-backend"), "");

    let mut child = command(&dir, "server-a")
        .args(["--session-shell", "plain"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while output(&dir, "server-a", "--session-backend") != "plain" {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("session did not become live");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(output(&dir, "server-b", "--session-backend"), "");
    child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    assert!(child.wait().unwrap().success());
    // The stale JSON is still there, but it must no longer describe a live shell.
    assert_eq!(output(&dir, "server-a", "--session-backend"), "");
}

#[test]
fn view_input_eof_does_not_send_eof_to_the_embedded_shell() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_owned();
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::write(dir.join("socket"), "").unwrap();
    fs::write(
        dir.join("herdr"),
        "#!/bin/sh\nprintf '%s\\n' '{\"result\":{\"pane\":{\"pane_id\":\"w1:p1\",\"terminal_id\":\"t1\"}}}'\n",
    )
    .unwrap();
    fs::set_permissions(dir.join("herdr"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        dir.join("scripts/floating-shell.sh"),
        "exec \"$FLOAX_BIN\" --session-shell plain\n",
    )
    .unwrap();
    fs::write(
        dir.join("shell"),
        "#!/bin/sh\ntouch \"$HERDR_PLUGIN_ROOT/ready\"\nread -r line\ntouch \"$HERDR_PLUGIN_ROOT/exited\"\n",
    )
    .unwrap();
    fs::set_permissions(dir.join("shell"), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(Command::new("mkfifo")
        .arg(dir.join("input"))
        .status()
        .unwrap()
        .success());

    // The application keeps a controlling terminal, but its input can close
    // independently, just as it can during Herdr's asynchronous pane teardown.
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["-c", "exec \"$FLOAX_BIN\" < \"$FLOAX_INPUT\""]);
    for (name, value) in command(&dir, "socket").get_envs() {
        cmd.env(name, value.unwrap());
    }
    cmd.env("FLOAX_BIN", env!("CARGO_BIN_EXE_herdr-floax"));
    cmd.env("FLOAX_INPUT", dir.join("input"));
    cmd.env("HERDR_PLUGIN_ROOT", &dir);
    cmd.env("HERDR_BIN_PATH", dir.join("herdr"));
    cmd.env("HERDR_PANE_ID", "w1:p1");
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let input = fs::OpenOptions::new()
        .write(true)
        .open(dir.join("input"))
        .unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let drain = std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dir.join("ready").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = dir.join("ready").exists();
    drop(input);
    std::thread::sleep(Duration::from_millis(300));
    let exited = dir.join("exited").exists();
    let _ = child.kill();
    let _ = child.wait();
    drop(pair.master);
    drain.join().unwrap();
    assert!(started, "embedded shell never started");
    assert!(!exited, "view input EOF must not exit the embedded shell");
}

// Real Herdr is needed: a mocked zoom response cannot reproduce the host's
// asynchronous split teardown. Run with `cargo test --test sessions -- --ignored`.
#[test]
#[ignore = "requires herdr on PATH; starts an isolated server and PTY client"]
fn picker_keeps_selected_pane_fullscreen_after_teardown() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use serde_json::Value;
    use std::os::unix::fs::symlink;

    struct Server {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        stop: Command,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.stop.output();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let plugin = dir.join("plugin");
    fs::create_dir_all(plugin.join("target/release")).unwrap();
    fs::create_dir_all(plugin.join("scripts")).unwrap();
    fs::write(
        plugin.join("herdr-plugin.toml"),
        include_str!("../herdr-plugin.toml"),
    )
    .unwrap();
    symlink(
        env!("CARGO_BIN_EXE_herdr-floax"),
        plugin.join("target/release/herdr-floax"),
    )
    .unwrap();
    // Use a plain shell so this test needs no multiplexer and leaves no detached
    // shells behind. Selecting an open view exercises the same teardown race.
    fs::write(
        plugin.join("scripts/floating-shell.sh"),
        "exec \"$HERDR_PLUGIN_ROOT/target/release/herdr-floax\" --session-shell plain\n",
    )
    .unwrap();
    let herdr = || {
        let mut command = Command::new("herdr");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("HERDR_") {
                command.env_remove(name);
            }
        }
        command
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("HERDR_CONFIG_PATH", dir.join("config/herdr/config.toml"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm-256color");
        command
    };
    let api = |args: &[&str]| -> Value {
        let out = herdr().args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        // send-keys returns plain text; inventory/layout return JSON.
        serde_json::from_slice::<Value>(&out.stdout).unwrap_or(Value::Null)["result"].clone()
    };
    api(&["plugin", "link", plugin.to_str().unwrap()]);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new("herdr");
    for (name, value) in herdr().get_envs() {
        if let Some(value) = value {
            cmd.env(name, value);
        } else {
            cmd.env_remove(name);
        }
    }
    let mut stop = herdr();
    stop.args(["server", "stop"]);
    let _server = Server {
        child: pair.slave.spawn_command(cmd).unwrap(),
        stop,
    };
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let drain = std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    let wait = |check: &mut dyn FnMut() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "Herdr state did not converge");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait(&mut || {
        herdr()
            .args(["pane", "list"])
            .output()
            .unwrap()
            .status
            .success()
    });
    api(&[
        "workspace",
        "create",
        "--cwd",
        dir.to_str().unwrap(),
        "--focus",
    ]);
    let find = |label: &str| -> Option<String> {
        api(&["pane", "list"])["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["label"] == label)
            .map(|p| p["pane_id"].as_str().unwrap().to_owned())
    };
    api(&["plugin", "action", "invoke", "herdr-floax.toggle"]);
    wait(&mut || find("⌂ floax").is_some());
    let floating = find("⌂ floax").unwrap();
    api(&["plugin", "action", "invoke", "herdr-floax.instances"]);
    wait(&mut || find("Floax instances").is_some());
    let picker = find("Floax instances").unwrap();
    api(&[
        "pane",
        "wait-output",
        &picker,
        "--match",
        "running sessions",
        "--timeout",
        "5000",
    ]);
    // Refresh in case the first inventory preceded shell startup.
    api(&["pane", "send-keys", &picker, "r"]);
    api(&[
        "pane",
        "wait-output",
        &picker,
        "--match",
        "1 running sessions",
        "--timeout",
        "5000",
    ]);
    api(&["pane", "send-keys", &picker, "Enter"]);
    wait(&mut || find("Floax instances").is_none());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let layout = api(&["pane", "layout", "--pane", &floating]);
        if layout["layout"]["zoomed"] == true && layout["layout"]["focused_pane_id"] == floating {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Selected Floax must remain fullscreen after picker removal: {layout}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(_server);
    drop(pair.master);
    drain.join().unwrap();
}
