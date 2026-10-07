//! Session-local named bar instances and their private control socket.

use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, BufRead, BufReader, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(5);

/// A control endpoint keyed by runtime directory, Wayland socket and instance.
#[derive(Clone, Debug)]
pub struct Identity {
    socket: PathBuf,
    lock: PathBuf,
    session: Vec<u8>,
    name: String,
}

impl Identity {
    /// Resolve the current session. No cross-session or process-name fallback.
    pub fn current(name: &str) -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is required"))?;
        let display = std::env::var_os("WAYLAND_DISPLAY")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| io::Error::other("WAYLAND_DISPLAY is required"))?;
        let display = PathBuf::from(display);
        let session = if display.is_absolute() {
            display
        } else {
            PathBuf::from(&runtime).join(display)
        };
        Self::at(Path::new(&runtime), &session, name)
    }

    fn at(runtime: &Path, session: impl AsRef<Path>, name: &str) -> io::Result<Self> {
        if name.is_empty()
            || name.len() > 48
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(io::Error::other("invalid instance name"));
        }
        let dir = runtime.join("tablero");
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        use std::os::unix::fs::MetadataExt;
        let meta = fs::symlink_metadata(&dir)?;
        if !meta.is_dir()
            || meta.uid() != rustix::process::getuid().as_raw()
            || meta.permissions().mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "tablero runtime directory must be owned by you with mode 0700",
            ));
        }
        let session = session.as_ref().as_os_str().as_bytes().to_vec();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        (&session, name).hash(&mut hash);
        let key = format!("{:016x}", hash.finish());
        Ok(Self {
            socket: dir.join(format!("{key}.sock")),
            lock: dir.join(format!("{key}.lock")),
            session,
            name: name.into(),
        })
    }

    /// Serialize restarts of this instance without blocking other names or sessions.
    pub fn lock_restart(&self) -> io::Result<File> {
        let file = lock_file(&self.lock.with_extension("restart-lock"))?;
        file.try_lock()
            .map_err(|e| io::Error::other(format!("another restart is in progress: {e}")))?;
        Ok(file)
    }

    /// Unique private startup endpoint, removed by the restart command.
    pub fn startup_socket(&self) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        self.socket
            .with_file_name(format!("start-{}-{nonce:x}.sock", std::process::id()))
    }

    /// Whether the instance lock is free. Lock files are never unlinked.
    pub fn is_stopped(&self) -> io::Result<bool> {
        let file = lock_file(&self.lock)?;
        match file.try_lock() {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

fn lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

/// Requests understood by the bar's event loop.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Operation {
    Status,
    Reload,
    Stop,
}

#[derive(Serialize, Deserialize)]
struct WireRequest {
    session: Vec<u8>,
    name: String,
    operation: Operation,
}

/// Startup metadata retained by the target, including non-UTF8 filesystem paths.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metadata {
    config: Option<Vec<u8>>,
    executable: Vec<u8>,
}
impl Metadata {
    /// Absolute configuration path used by this instance.
    pub fn config_path(&self) -> Option<PathBuf> {
        self.config
            .clone()
            .map(|p| std::ffi::OsString::from_vec(p).into())
    }
    /// Executable used to start this instance (also used for its replacement).
    pub fn executable(&self) -> PathBuf {
        std::ffi::OsString::from_vec(self.executable.clone()).into()
    }
}

/// A reply is sent only after the requested event-loop action succeeds.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    /// A user-facing failure; absent on success.
    pub error: Option<String>,
    /// The running instance's startup parameters.
    pub metadata: Metadata,
}

pub(crate) struct Incoming {
    pub operation: Operation,
    stream: UnixStream,
}
impl Incoming {
    pub(crate) fn reply(mut self, metadata: &Metadata, result: Result<(), String>) {
        let response = Response {
            error: result.err(),
            metadata: metadata.clone(),
        };
        let _ = write_message(&mut self.stream, &response);
    }
}

fn write_message(stream: &mut UnixStream, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, value)?;
    stream.write_all(b"\n")
}
fn read_message<T: serde::de::DeserializeOwned>(stream: &UnixStream) -> io::Result<T> {
    use std::io::Read;
    let mut bytes = Vec::new();
    BufReader::new(stream.take(8193)).read_until(b'\n', &mut bytes)?;
    if bytes.len() > 8192 || bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid lifecycle message"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Send a bounded request to precisely this session and named instance.
pub fn request(identity: &Identity, operation: Operation) -> io::Result<Response> {
    let mut stream = UnixStream::connect(&identity.socket)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    write_message(
        &mut stream,
        &WireRequest {
            session: identity.session.clone(),
            name: identity.name.clone(),
            operation,
        },
    )?;
    read_message(&stream)
}

/// Owns the instance lock and socket for the entire lifetime of the bar.
pub struct Instance {
    identity: Identity,
    _lock: File,
    listener: Option<UnixListener>,
    pub(crate) metadata: Metadata,
    stopping: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    ready_notifier: Option<UnixStream>,
}
impl Instance {
    /// Claim a named instance before opening any Wayland surfaces.
    pub fn claim(identity: Identity, config: Option<PathBuf>) -> io::Result<Self> {
        let lock = lock_file(&identity.lock)?;
        lock.try_lock().map_err(|e| {
            io::Error::other(format!(
                "instance '{}' is already running or starting: {e}; use reload or restart",
                identity.name
            ))
        })?;
        match fs::remove_file(&identity.socket) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(&identity.socket)?;
        fs::set_permissions(&identity.socket, fs::Permissions::from_mode(0o600))?;
        let config = config
            .map(std::path::absolute)
            .transpose()?
            .map(|p| p.as_os_str().as_bytes().to_vec());
        let executable = std::env::current_exe()?.as_os_str().as_bytes().to_vec();
        Ok(Self {
            identity,
            _lock: lock,
            listener: Some(listener),
            metadata: Metadata { config, executable },
            stopping: Arc::new(AtomicBool::new(false)),
            worker: None,
            ready_notifier: None,
        })
    }

    /// Notify a restarting parent only after the event loop has been initialized.
    pub fn with_ready_notifier(mut self, stream: UnixStream) -> Self {
        self.ready_notifier = Some(stream);
        self
    }

    pub(crate) fn notify_ready(&mut self) -> io::Result<()> {
        if let Some(mut stream) = self.ready_notifier.take() {
            stream.write_all(b"OK\n")?;
        }
        Ok(())
    }

    pub(crate) fn listen(&mut self) -> io::Result<calloop::channel::Channel<Incoming>> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| io::Error::other("control listener already started"))?;
        let identity = self.identity.clone();
        let stopping = self.stopping.clone();
        let (tx, rx) = calloop::channel::channel();
        self.worker = Some(
            thread::Builder::new()
                .name("tablero-control".into())
                .spawn(move || {
                    for stream in listener.incoming() {
                        if stopping.load(Ordering::Acquire) {
                            break;
                        }
                        let Ok(stream) = stream else {
                            break;
                        };
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                        if let Ok(request) = read_message::<WireRequest>(&stream)
                            && request.session == identity.session
                            && request.name == identity.name
                            && tx
                                .send(Incoming {
                                    operation: request.operation,
                                    stream,
                                })
                                .is_err()
                        {
                            break;
                        }
                    }
                })?,
        );
        Ok(rx)
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if self.worker.is_some() {
            let _ = UnixStream::connect(&self.identity.socket);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.identity.socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_are_acknowledged_by_event_loop_and_return_errors() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::at(dir.path(), "wayland-1", "dev").unwrap();
        let config = dir.path().join("dev.toml");
        let mut instance = Instance::claim(identity.clone(), Some(config.clone())).unwrap();
        let channel = instance.listen().unwrap();
        let metadata = instance.metadata.clone();
        let mut event_loop = calloop::EventLoop::<bool>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(channel, move |event, _, handled| {
                if let calloop::channel::Event::Msg(incoming) = event {
                    assert!(matches!(incoming.operation, Operation::Reload));
                    *handled = true;
                    incoming.reply(&metadata, Err("invalid theme".into()));
                }
            })
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let client = thread::spawn(move || tx.send(request(&identity, Operation::Reload)).unwrap());
        assert!(
            rx.recv_timeout(Duration::from_millis(30)).is_err(),
            "no acknowledgment before dispatch"
        );
        let mut handled = false;
        event_loop
            .dispatch(Duration::from_secs(1), &mut handled)
            .unwrap();
        assert!(handled);
        let response = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert_eq!(response.error.as_deref(), Some("invalid theme"));
        assert_eq!(response.metadata.config_path(), Some(config));
        client.join().unwrap();
    }

    #[test]
    fn development_and_default_instances_coexist_with_independent_configs() {
        let dir = tempfile::tempdir().unwrap();
        let normal_id = Identity::at(dir.path(), "wayland-1", "default").unwrap();
        let dev_id = Identity::at(dir.path(), "wayland-1", "dev").unwrap();
        let normal_config = dir.path().join("normal.toml");
        let dev_config = dir.path().join("dev.toml");
        let normal = Instance::claim(normal_id.clone(), Some(normal_config.clone())).unwrap();
        let dev = Instance::claim(dev_id.clone(), Some(dev_config.clone())).unwrap();
        assert_eq!(normal.metadata.config_path(), Some(normal_config));
        assert_eq!(dev.metadata.config_path(), Some(dev_config));
        assert!(!normal_id.is_stopped().unwrap());
        assert!(!dev_id.is_stopped().unwrap());
        let normal_restart = normal_id.lock_restart().unwrap();
        assert!(normal_id.lock_restart().is_err());
        assert!(dev_id.lock_restart().is_ok());
        drop(normal_restart);
        drop(dev);
        assert!(dev_id.is_stopped().unwrap());
        assert!(!normal_id.is_stopped().unwrap());
        assert!(normal_id.socket.exists());
    }

    #[test]
    fn identity_separates_sessions_and_names() {
        let dir = tempfile::tempdir().unwrap();
        let a = Identity::at(dir.path(), "wayland-1", "default").unwrap();
        assert_ne!(
            a.socket,
            Identity::at(dir.path(), "wayland-2", "default")
                .unwrap()
                .socket
        );
        assert_ne!(
            a.socket,
            Identity::at(dir.path(), "wayland-1", "dev").unwrap().socket
        );
    }
    #[test]
    fn lock_prevents_duplicates_and_releases_after_exit() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::at(dir.path(), "wayland-1", "default").unwrap();
        let instance = Instance::claim(identity.clone(), None).unwrap();
        assert!(Instance::claim(identity.clone(), None).is_err());
        drop(instance);
        assert!(Instance::claim(identity, None).is_ok());
    }
    #[test]
    fn stale_socket_is_recovered_only_after_claiming_lock() {
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity::at(dir.path(), "wayland-1", "dev").unwrap();
        let stale = std::os::unix::net::UnixListener::bind(&identity.socket).unwrap();
        drop(stale);
        let instance = Instance::claim(identity.clone(), None).unwrap();
        assert!(identity.socket.exists());
        drop(instance);
        assert!(!identity.socket.exists());
    }
    #[test]
    fn unsafe_runtime_directory_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("tablero")).unwrap();
        std::fs::set_permissions(
            dir.path().join("tablero"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(Identity::at(dir.path(), "wayland-1", "dev").is_err());
    }
}
