//! Native Hypridle process discovery, polling, and state control.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::warn;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::fs;
use tokio::process::Command as TokioCommand;
use tokio::time;

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};
use crate::widget::{Command, Hypridle, Msg};

const PROC_ROOT: &str = "/proc";
const HYPRIDLE_EXECUTABLE: &str = "hypridle";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HypridleAction {
    None,
    Start,
    Stop,
}

fn required_action(active: bool, desired: bool) -> HypridleAction {
    match (active, desired) {
        (false, true) => HypridleAction::Start,
        (true, false) => HypridleAction::Stop,
        _ => HypridleAction::None,
    }
}

/// Find same-user processes whose kernel command name is exactly `hypridle`.
async fn discover_hypridle_processes(root: &Path) -> io::Result<Vec<i32>> {
    let current_uid = fs::metadata(root.join("self"))
        .await
        .or_else(|_| std::fs::metadata(root))?
        .uid();
    let mut entries = fs::read_dir(root).await?;
    let mut pids = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if metadata.uid() != current_uid {
            continue;
        }
        let Ok(comm) = fs::read_to_string(path.join("comm")).await else {
            continue;
        };
        if comm.trim_end() == HYPRIDLE_EXECUTABLE {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

async fn active(root: &Path) -> io::Result<bool> {
    Ok(!discover_hypridle_processes(root).await?.is_empty())
}

/// Lightweight process poller used only when the Hypridle widget is configured.
pub struct HypridleProducer {
    root: PathBuf,
    interval: Duration,
}

impl HypridleProducer {
    /// Poll the session's process table every two seconds.
    pub fn new() -> Self {
        Self {
            root: PathBuf::from(PROC_ROOT),
            interval: POLL_INTERVAL,
        }
    }
}

impl Default for HypridleProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for HypridleProducer {
    fn name(&self) -> String {
        "hypridle".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run_producer(self.root, self.interval, tx))
    }
}

async fn run_producer(root: PathBuf, interval: Duration, tx: MsgSender) -> ProducerResult {
    let mut previous = None;
    loop {
        let next = match active(&root).await {
            Ok(active) => active,
            Err(error) => {
                warn!("hypridle: reading process state failed: {error}");
                time::sleep(interval).await;
                continue;
            }
        };
        if previous != Some(next) {
            previous = Some(next);
            if tx.send(Msg::Hypridle(Hypridle::new(next))).is_err() {
                return Ok(());
            }
        }
        time::sleep(interval).await;
    }
}

fn signal_stop(pids: &[i32]) -> io::Result<()> {
    for &pid in pids {
        if let Err(error) = kill(Pid::from_raw(pid), Signal::SIGTERM)
            && error != Errno::ESRCH
        {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    Ok(())
}

async fn wait_until_stopped(root: &Path) -> io::Result<bool> {
    let deadline = time::Instant::now() + STOP_TIMEOUT;
    loop {
        if !active(root).await? {
            return Ok(false);
        }
        if time::Instant::now() >= deadline {
            return Ok(true);
        }
        time::sleep(STOP_POLL_INTERVAL).await;
    }
}

async fn set_state(root: &Path, executable: &Path, desired: bool) -> io::Result<bool> {
    let pids = discover_hypridle_processes(root).await?;
    match required_action(!pids.is_empty(), desired) {
        HypridleAction::None => Ok(desired),
        HypridleAction::Start => {
            TokioCommand::new(executable).spawn()?;
            Ok(true)
        }
        HypridleAction::Stop => {
            signal_stop(&pids)?;
            wait_until_stopped(root).await
        }
    }
}

/// Execute typed Hypridle state requests and immediately report the result.
pub async fn run_commands(mut commands: CommandReceiver, updates: MsgSender) -> ProducerResult {
    while let Some(command) = commands.recv().await {
        let Command::SetHypridle(desired) = command else {
            continue;
        };
        let state = match set_state(
            Path::new(PROC_ROOT),
            Path::new(HYPRIDLE_EXECUTABLE),
            desired,
        )
        .await
        {
            Ok(state) => state,
            Err(error) => {
                warn!("hypridle: setting active={desired} failed: {error}");
                match active(Path::new(PROC_ROOT)).await {
                    Ok(state) => state,
                    Err(refresh_error) => {
                        warn!("hypridle: refreshing after command failed: {refresh_error}");
                        continue;
                    }
                }
            }
        };
        if updates.send(Msg::Hypridle(Hypridle::new(state))).is_err() {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{HypridleAction, discover_hypridle_processes, required_action};

    fn process(root: &std::path::Path, pid: i32, name: &str) {
        let path = root.join(pid.to_string());
        fs::create_dir(&path).unwrap();
        fs::write(path.join("comm"), format!("{name}\n")).unwrap();
    }

    #[test]
    fn discovery_matches_exact_same_user_process_names() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("self")).unwrap();
        process(root.path(), 42, "hypridle");
        process(root.path(), 43, "hypridle-helper");
        process(root.path(), 44, "Hypridle");

        let pids = runtime
            .block_on(discover_hypridle_processes(root.path()))
            .unwrap();
        assert_eq!(pids, vec![42]);
    }

    #[test]
    fn discovery_ignores_non_process_entries_and_missing_comm_files() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("self")).unwrap();
        fs::create_dir(root.path().join("51")).unwrap();
        process(root.path(), 52, "hypridle");

        let pids = runtime
            .block_on(discover_hypridle_processes(root.path()))
            .unwrap();
        assert_eq!(pids, vec![52]);
    }

    #[test]
    fn desired_state_is_idempotent() {
        assert_eq!(required_action(false, false), HypridleAction::None);
        assert_eq!(required_action(true, true), HypridleAction::None);
        assert_eq!(required_action(false, true), HypridleAction::Start);
        assert_eq!(required_action(true, false), HypridleAction::Stop);
    }
}
