//! Native Linux backlight producer and systemd-logind command executor.

use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::widget::{Backlight, Command, Msg, ScrollDirection};
use log::warn;
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use tokio::fs;
use tokio::sync::mpsc;
use tokio::time;
use zbus::{Connection, proxy};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

const SYSFS_BACKLIGHT: &str = "/sys/class/backlight";
/// Re-read cadence when no udev monitor is available to report changes.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Re-read cadence while udev reports changes: only a net under a lost event.
const RESYNC_INTERVAL: Duration = Duration::from_secs(60);
const UDEV_TOKEN: Token = Token(0);
const SHUTDOWN_TOKEN: Token = Token(1);

#[proxy(interface = "org.freedesktop.login1.Session")]
trait LoginSession {
    /// Set an absolute kernel backlight value for an active login session.
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;
}

/// A lightweight polling producer for Linux screen-backlight devices.
pub struct BacklightProducer {
    root: PathBuf,
    interval: Duration,
}

impl BacklightProducer {
    /// Poll the standard Linux backlight sysfs class every two seconds.
    pub fn new() -> Self {
        Self {
            root: PathBuf::from(SYSFS_BACKLIGHT),
            interval: POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    fn at(root: PathBuf, interval: Duration) -> Self {
        Self { root, interval }
    }
}

impl Default for BacklightProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for BacklightProducer {
    fn name(&self) -> String {
        "linux-backlight".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run_producer(self.root, self.interval, tx))
    }
}

async fn run_producer(root: PathBuf, interval: Duration, tx: MsgSender) -> ProducerResult {
    let mut events = if root == Path::new(SYSFS_BACKLIGHT) {
        Some(spawn_udev_monitor()?)
    } else {
        None
    };
    let mut previous = None;
    loop {
        let devices = match read_backlights(&root).await {
            Ok(devices) => devices,
            Err(error) => {
                warn!("backlight: reading {} failed: {error}", root.display());
                Vec::new()
            }
        };
        if previous.as_ref() != Some(&devices) {
            previous = Some(devices.clone());
            if tx.send(Msg::Backlight(devices)).is_err() {
                return Ok(());
            }
        }
        if let Some(monitor) = &mut events {
            let resync = interval.max(RESYNC_INTERVAL);
            if matches!(
                time::timeout(resync, monitor.changes.recv()).await,
                Ok(None)
            ) {
                events = None;
            }
        } else {
            time::sleep(interval).await;
        }
    }
}

/// Change notifications from the udev monitor thread, which is told to stop
/// when this is dropped instead of waking periodically to find out.
struct UdevMonitor {
    changes: mpsc::UnboundedReceiver<()>,
    shutdown: Arc<Waker>,
}

impl Drop for UdevMonitor {
    fn drop(&mut self) {
        // Nothing to do on failure: the thread then ends with its next event.
        let _ = self.shutdown.wake();
    }
}

fn spawn_udev_monitor() -> io::Result<UdevMonitor> {
    let (tx, changes) = mpsc::unbounded_channel();
    let mut poll = Poll::new()?;
    let shutdown = Arc::new(Waker::new(poll.registry(), SHUTDOWN_TOKEN)?);
    thread::Builder::new()
        .name("tablero-backlight".to_string())
        .spawn(move || {
            let mut result = || -> io::Result<()> {
                // Not `Send`, so the socket lives and dies on this thread.
                let socket = udev::MonitorBuilder::new()?
                    .match_subsystem("backlight")?
                    .listen()?;
                let mut events = Events::with_capacity(8);
                let fd = socket.as_raw_fd();
                poll.registry()
                    .register(&mut SourceFd(&fd), UDEV_TOKEN, Interest::READABLE)?;
                loop {
                    if let Err(error) = poll.poll(&mut events, None) {
                        if error.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(error);
                    }
                    if events.iter().any(|event| event.token() == SHUTDOWN_TOKEN) {
                        return Ok(());
                    }
                    // Drain the queue: a burst is answered by one re-read.
                    if socket.iter().count() > 0 && tx.send(()).is_err() {
                        return Ok(());
                    }
                }
            };
            if let Err(error) = result() {
                warn!("backlight: udev monitor failed: {error}");
            }
        })?;
    Ok(UdevMonitor { changes, shutdown })
}

/// Read and normalize every usable device below a backlight sysfs class path.
pub async fn read_backlights(root: &Path) -> io::Result<Vec<Backlight>> {
    let mut entries = match fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut devices = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        match read_device(&entry.path(), &name).await {
            Ok(device) => devices.push(device),
            Err(error) => warn!("backlight: ignoring device {name:?}: {error}"),
        }
    }
    devices.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(devices)
}

async fn read_device(path: &Path, name: &str) -> io::Result<Backlight> {
    let max_brightness = read_u32(&path.join("max_brightness")).await?;
    let preferred_attribute = brightness_attribute(name);
    let brightness = match read_u32(&path.join(preferred_attribute)).await {
        Ok(value) => value,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound && preferred_attribute != "brightness" =>
        {
            read_u32(&path.join("brightness")).await?
        }
        Err(error) => return Err(error),
    };
    if max_brightness == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "max_brightness is zero",
        ));
    }
    Ok(Backlight::new(name, brightness, max_brightness))
}

fn brightness_attribute(name: &str) -> &'static str {
    if name.starts_with("amdgpu_bl") || name == "apple-panel-bl" {
        "brightness"
    } else {
        "actual_brightness"
    }
}

async fn read_u32(path: &Path) -> io::Result<u32> {
    let value = fs::read_to_string(path).await?;
    value.trim().parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not an unsigned integer: {error}", path.display()),
        )
    })
}

/// Calculate an absolute brightness from a fresh raw value and percentage step.
pub fn adjusted_brightness(
    current: u32,
    maximum: u32,
    direction: ScrollDirection,
    step_percent: f64,
) -> u32 {
    if maximum == 0 {
        return 0;
    }
    let step = ((maximum as f64 * step_percent / 100.0).round() as u32).max(1);
    match direction {
        ScrollDirection::Increase => current.saturating_add(step).min(maximum),
        ScrollDirection::Decrease => current.saturating_sub(step),
    }
}

/// Drain backlight commands and execute them through systemd-logind.
pub async fn run_commands(mut commands: CommandReceiver, updates: MsgSender) -> ProducerResult {
    let connection = match Connection::system().await {
        Ok(connection) => Some(connection),
        Err(error) => {
            warn!("backlight: system bus unavailable; scrolling disabled: {error}");
            None
        }
    };
    while let Some(command) = commands.recv().await {
        let Command::AdjustBacklight {
            device,
            direction,
            step,
        } = command
        else {
            continue;
        };
        let Some(connection) = &connection else {
            continue;
        };
        if let Err(error) = adjust_device(
            connection,
            Path::new(SYSFS_BACKLIGHT),
            &device,
            direction,
            step,
        )
        .await
        {
            warn!("backlight: adjusting {device:?} failed: {error}");
            continue;
        }
        match read_backlights(Path::new(SYSFS_BACKLIGHT)).await {
            Ok(devices) => {
                if updates.send(Msg::Backlight(devices)).is_err() {
                    return Ok(());
                }
            }
            Err(error) => warn!("backlight: refreshing after adjustment failed: {error}"),
        }
    }
    Ok(())
}

/// True when `name` is a single sysfs device segment (no `/`, `.`, or `..`).
///
/// Device names come from our own enumeration in the normal path; this guard
/// still rejects traversal if a future caller ever threaded a free-form string
/// into the logind brightness adjust path.
pub fn is_safe_backlight_device(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

async fn adjust_device(
    connection: &Connection,
    root: &Path,
    device: &str,
    direction: ScrollDirection,
    step: f64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !is_safe_backlight_device(device) {
        return Err(format!("invalid backlight device name: {device:?}").into());
    }
    let path = root.join(device);
    let current = read_u32(&path.join("brightness")).await?;
    let maximum = read_u32(&path.join("max_brightness")).await?;
    let next = adjusted_brightness(current, maximum, direction, step);

    let proxy = match LoginSessionProxy::builder(connection)
        .destination("org.freedesktop.login1")?
        .path("/org/freedesktop/login1/session/auto")?
        .build()
        .await
    {
        Ok(proxy) => proxy,
        Err(_) => {
            LoginSessionProxy::builder(connection)
                .destination("org.freedesktop.login1")?
                .path("/org/freedesktop/login1/session/self")?
                .build()
                .await?
        }
    };
    proxy
        .set_brightness("backlight", device, next)
        .await
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;

    fn device(root: &Path, name: &str, brightness: &str, actual: Option<&str>, maximum: &str) {
        let path = root.join(name);
        stdfs::create_dir(&path).unwrap();
        stdfs::write(path.join("brightness"), brightness).unwrap();
        if let Some(actual) = actual {
            stdfs::write(path.join("actual_brightness"), actual).unwrap();
        }
        stdfs::write(path.join("max_brightness"), maximum).unwrap();
    }

    #[test]
    fn percentage_steps_round_clamp_and_never_stall() {
        assert_eq!(
            adjusted_brightness(50, 100, ScrollDirection::Increase, 1.0),
            51
        );
        assert_eq!(
            adjusted_brightness(100, 100, ScrollDirection::Increase, 1.0),
            100
        );
        assert_eq!(
            adjusted_brightness(0, 100, ScrollDirection::Decrease, 1.0),
            0
        );
        assert_eq!(
            adjusted_brightness(1, 10, ScrollDirection::Increase, 1.0),
            2
        );
    }

    #[test]
    fn sysfs_devices_are_sorted_and_actual_brightness_is_preferred() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        device(temp.path(), "panel-b", "50", Some("40"), "100");
        device(temp.path(), "panel-a", "20", None, "100");
        let values = runtime.block_on(read_backlights(temp.path())).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].name(), "panel-a");
        assert_eq!(values[0].percent(), 20);
        assert_eq!(values[1].name(), "panel-b");
        assert_eq!(values[1].percent(), 40);
    }

    #[test]
    fn amd_and_apple_panels_match_waybars_brightness_source() {
        assert_eq!(brightness_attribute("amdgpu_bl1"), "brightness");
        assert_eq!(brightness_attribute("apple-panel-bl"), "brightness");
        assert_eq!(brightness_attribute("intel_backlight"), "actual_brightness");
    }

    #[test]
    fn amd_panel_uses_requested_brightness_instead_of_actual_brightness() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        device(temp.path(), "amdgpu_bl1", "85", Some("81"), "100");
        let values = runtime.block_on(read_backlights(temp.path())).unwrap();
        assert_eq!(values[0].percent(), 85);
    }

    #[test]
    fn malformed_devices_are_skipped_without_hiding_valid_ones() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let temp = tempfile::tempdir().unwrap();
        device(temp.path(), "good", "50", None, "100");
        device(temp.path(), "bad", "wat", None, "100");
        let values = runtime.block_on(read_backlights(temp.path())).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].name(), "good");
    }

    #[test]
    fn producer_can_be_constructed_against_a_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let producer = BacklightProducer::at(temp.path().to_path_buf(), Duration::from_millis(1));
        assert_eq!(producer.name(), "linux-backlight");
    }

    #[test]
    fn device_names_reject_path_traversal() {
        assert!(is_safe_backlight_device("intel_backlight"));
        assert!(is_safe_backlight_device("amdgpu_bl1"));
        assert!(!is_safe_backlight_device(""));
        assert!(!is_safe_backlight_device("."));
        assert!(!is_safe_backlight_device(".."));
        assert!(!is_safe_backlight_device("../brightness"));
        assert!(!is_safe_backlight_device("foo/bar"));
        assert!(!is_safe_backlight_device("foo\0bar"));
    }

    #[test]
    fn dropping_the_monitor_stops_its_thread_without_a_poll_timeout() {
        let mut monitor = spawn_udev_monitor().unwrap();
        let shutdown = monitor.shutdown.clone();
        let (done, stopped) = std::sync::mpsc::channel();
        thread::spawn(move || {
            // The thread owns the only sender, so `None` means it has returned.
            while monitor.changes.blocking_recv().is_some() {}
            done.send(()).unwrap();
        });
        if stopped.recv_timeout(Duration::from_millis(100)).is_ok() {
            eprintln!("skipped: no udev monitor socket in this environment");
            return;
        }

        // What `Drop` does; the monitor itself is busy being waited on above.
        shutdown.wake().unwrap();
        stopped
            .recv_timeout(Duration::from_secs(5))
            .expect("monitor thread stopped");
    }
}
