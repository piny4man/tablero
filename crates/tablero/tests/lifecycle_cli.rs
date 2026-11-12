use std::process::{Command, Output};

fn command(runtime: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tablero"))
        .env("XDG_RUNTIME_DIR", runtime)
        // restart logs the bar here; never touch the real ~/.local/state.
        .env("XDG_STATE_HOME", runtime.join("state"))
        .env("WAYLAND_DISPLAY", "tablero-test-no-compositor")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn restart_reports_child_startup_failure_instead_of_success() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "height = 32").unwrap();
    let result = command(
        dir.path(),
        &["restart", "--config", config.to_str().unwrap()],
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(stderr.contains("startup failed"), "{stderr}");
    assert!(!stderr.contains("not available"), "{stderr}");
    // The replacement's own error is kept for diagnosis instead of /dev/null.
    let log = std::fs::read_to_string(dir.path().join("state/tablero/default.log")).unwrap();
    assert!(log.contains("wayland"), "{log}");
}

#[test]
fn reload_stopped_instance_suggests_restart() {
    let dir = tempfile::tempdir().unwrap();
    let result = command(dir.path(), &["reload", "--instance", "dev"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("tablero restart --instance dev"));
}

#[test]
fn restart_with_invalid_config_never_stops_the_running_instance() {
    use std::{
        fs::OpenOptions,
        io::{BufRead, BufReader, Write},
        os::unix::{ffi::OsStrExt, net::UnixListener},
    };
    let dir = tempfile::tempdir().unwrap();
    // A stopped reload initializes this session/name's runtime identity.
    command(dir.path(), &["reload", "--instance", "dev"]);
    let lock_path = std::fs::read_dir(dir.path().join("tablero"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|v| v == "lock"))
        .unwrap();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    lock.lock().unwrap();
    let listener = UnixListener::bind(lock_path.with_extension("sock")).unwrap();
    let config = dir.path().join("invalid.toml");
    std::fs::write(&config, "height = 'invalid'").unwrap();
    let retained = config.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["operation"], "Status");
        let response = serde_json::json!({"error": null, "metadata": {
            "config": retained.as_os_str().as_bytes(),
            "executable": env!("CARGO_BIN_EXE_tablero").as_bytes()
        }});
        writeln!(stream, "{response}").unwrap();
        listener
    });
    let result = command(dir.path(), &["restart", "--instance", "dev"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("invalid.toml"));
    let listener = server.join().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(
        matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "no Stop request may follow failed validation"
    );
    let other = OpenOptions::new()
        .read(true)
        .write(true)
        .open(lock_path)
        .unwrap();
    assert!(matches!(
        other.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
}

#[test]
fn help_does_not_require_a_wayland_session() {
    let result = Command::new(env!("CARGO_BIN_EXE_tablero"))
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("XDG_RUNTIME_DIR")
        .arg("--help")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("reload|restart"));
}

/// Regression (NUKE-171): a bar started by `tablero restart` used to leak its
/// one-shot readiness socket into every click. `tablero tui` then answered a
/// handshake meant for the bar (or failed when the socket was gone) before it
/// ever opened a panel. Only the bar itself may connect to it.
#[test]
fn restart_handshake_is_answered_only_by_the_bar() {
    use std::os::unix::net::UnixListener;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("startup.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tablero"))
            .env("XDG_RUNTIME_DIR", dir.path())
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env("WAYLAND_DISPLAY", "tablero-test-no-compositor")
            .env("TABLERO_STARTUP_SOCKET", &socket)
            .args(args)
            .output()
            .unwrap()
    };
    let nobody_connected = |listener: &UnixListener| matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock);

    // No compositor here, so tui fails on Wayland, but must not touch the socket.
    run(&["tui", "true"]);
    assert!(
        nobody_connected(&listener),
        "tui answered the restart handshake"
    );
    let reload = run(&["reload", "--instance", "dev"]);
    assert!(String::from_utf8_lossy(&reload.stderr).contains("tablero restart --instance dev"));
    assert!(
        nobody_connected(&listener),
        "reload answered the restart handshake"
    );
    assert!(run(&["--help"]).status.success());
    assert!(
        nobody_connected(&listener),
        "help answered the restart handshake"
    );

    // The bar itself still reports through it (here: its Wayland failure).
    run(&["--instance", "dev"]);
    let (stream, _) = listener.accept().expect("bar reports startup");
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(stream), &mut line).unwrap();
    assert!(line.starts_with("ERR "), "{line}");
}
