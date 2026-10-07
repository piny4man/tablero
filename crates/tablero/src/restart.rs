//! Background replacement orchestration. Only the target socket can stop a bar.
use std::{
    error::Error,
    fs,
    io::{self, BufRead, BufReader, Read},
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tablero::lifecycle::{self, Identity, Operation};

pub(crate) const STARTUP_ENV: &str = "TABLERO_STARTUP_SOCKET";
const DEADLINE: Duration = Duration::from_secs(10);

struct StartupSocket(PathBuf);
impl Drop for StartupSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn restart(
    identity: &Identity,
    name: &str,
    override_path: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    let _guard = identity.lock_restart()?;
    let running = if identity.is_stopped()? {
        None
    } else {
        let response = lifecycle::request(identity, Operation::Status)?;
        if let Some(error) = response.error {
            return Err(error.into());
        }
        Some(response.metadata)
    };
    let path = select_config(override_path, running.as_ref())
        .map(std::path::absolute)
        .transpose()?;
    // Load before sending Stop: invalid config must leave the old bar untouched.
    super::load_config(path.as_deref())?;
    let executable = match &running {
        Some(m) => m.executable(),
        None => std::env::current_exe()?,
    };
    if running.is_some() {
        let response = lifecycle::request(identity, Operation::Stop)?;
        if let Some(error) = response.error {
            return Err(error.into());
        }
        let until = Instant::now() + DEADLINE;
        while !identity.is_stopped()? {
            if Instant::now() >= until {
                return Err("timed out waiting for shutdown; no replacement started".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    start_background(identity, name, path.as_deref(), &executable)
}

fn select_config(
    override_path: Option<PathBuf>,
    running: Option<&lifecycle::Metadata>,
) -> Option<PathBuf> {
    override_path.or_else(|| match running {
        Some(metadata) => metadata.config_path(),
        None => tablero::config::config_file_path(),
    })
}

fn start_background(
    identity: &Identity,
    name: &str,
    path: Option<&Path>,
    executable: &Path,
) -> Result<(), Box<dyn Error>> {
    let socket = StartupSocket(identity.startup_socket());
    let listener = UnixListener::bind(&socket.0)?;
    listener.set_nonblocking(true)?;
    let mut command = Command::new(executable);
    command
        .arg("--instance")
        .arg(name)
        .env(STARTUP_ENV, &socket.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = path {
        command.arg("--config").arg(path);
    }
    let mut child = command.spawn()?;
    let result = wait_ready(&listener, &mut child, DEADLINE);
    if result.is_err() {
        // Kill only the replacement we spawned, never a PID found by name.
        let _ = child.kill();
        let _ = child.wait();
    }
    result.map_err(Into::into)
}

fn wait_ready(listener: &UnixListener, child: &mut Child, timeout: Duration) -> io::Result<()> {
    let until = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(
                    until
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                ))?;
                let mut line = String::new();
                BufReader::new(stream.take(8192)).read_line(&mut line)?;
                return match line.trim_end() {
                    "OK" => Ok(()),
                    error => Err(io::Error::other(format!("startup failed: {error}"))),
                };
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "startup failed: replacement exited with {status}"
            )));
        }
        if Instant::now() >= until {
            return Err(io::Error::other(
                "startup failed: timed out waiting for readiness",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::unix::net::UnixStream};

    #[test]
    fn config_selection_retains_target_path_and_accepts_override() {
        let metadata: lifecycle::Metadata = serde_json::from_value(serde_json::json!({
            "config": b"/target/dev.toml", "executable": b"/target/tablero"
        }))
        .unwrap();
        assert_eq!(
            select_config(None, Some(&metadata)),
            Some("/target/dev.toml".into())
        );
        assert_eq!(
            select_config(Some("/other.toml".into()), Some(&metadata)),
            Some("/other.toml".into())
        );
        let no_config: lifecycle::Metadata = serde_json::from_value(serde_json::json!({
            "config": null, "executable": b"/target/tablero"
        }))
        .unwrap();
        assert_eq!(select_config(None, Some(&no_config)), None);
    }

    #[test]
    fn startup_requires_explicit_readiness_and_propagates_failure() {
        for (message, success) in [("OK\n", true), ("ERR cannot connect to Wayland\n", false)] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("startup.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let mut child = Command::new("sleep").arg("5").spawn().unwrap();
            let writer = thread::spawn(move || {
                let mut stream = UnixStream::connect(socket).unwrap();
                stream.write_all(message.as_bytes()).unwrap();
            });
            let result = wait_ready(&listener, &mut child, Duration::from_secs(1));
            let _ = child.kill();
            child.wait().unwrap();
            writer.join().unwrap();
            assert_eq!(result.is_ok(), success);
            if !success {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("cannot connect to Wayland")
                );
            }
        }
    }

    #[test]
    fn child_without_readiness_times_out_or_reports_early_exit() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("startup.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let error = wait_ready(&listener, &mut child, Duration::from_millis(40)).unwrap_err();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(error.to_string().contains("timed out"));
        let mut child = Command::new("false").spawn().unwrap();
        assert!(
            wait_ready(&listener, &mut child, Duration::from_secs(1))
                .unwrap_err()
                .to_string()
                .contains("exited")
        );
        child.wait().unwrap();
    }
}
