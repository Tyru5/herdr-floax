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
