//! Arch Linux package-update source.
//!
//! Official repository updates come from `checkupdates`, which safely refreshes
//! a separate pacman database instead of leaving the system in a partial-upgrade
//! state. When `paru` is installed, `paru -Qua` adds pending AUR updates. Both
//! commands run directly without a shell and are bounded by a timeout.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use log::warn;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::time::{interval, timeout};

use tablero_core::widget::{Msg, PackageUpdate, PackageUpdates};

use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// A package-update command failure that prevents a trustworthy total.
#[derive(Debug)]
pub enum UpdatesError {
    /// The command could not be started.
    Spawn { program: PathBuf, source: io::Error },
    /// The command exceeded its execution deadline.
    Timeout { program: PathBuf },
    /// `checkupdates` returned a failure status other than its documented
    /// no-updates status (`2`).
    Failed {
        program: PathBuf,
        code: Option<i32>,
        stderr: String,
    },
}

impl fmt::Display for UpdatesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn { program, source } => {
                write!(f, "could not run {}: {source}", program.display())
            }
            Self::Timeout { program } => {
                write!(f, "{} timed out", program.display())
            }
            Self::Failed {
                program,
                code,
                stderr,
            } => {
                write!(
                    f,
                    "{} failed with status {}",
                    program.display(),
                    code.map_or_else(|| "signal".to_string(), |code| code.to_string())
                )?;
                if !stderr.trim().is_empty() {
                    write!(f, ": {}", stderr.trim())?;
                }
                Ok(())
            }
        }
    }
}

impl Error for UpdatesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Timeout { .. } | Self::Failed { .. } => None,
        }
    }
}

/// Count non-empty records in package-manager output.
pub fn count_output_lines(output: &[u8]) -> u32 {
    output
        .split(|byte| *byte == b'\n')
        .filter(|line| line.iter().any(|byte| !byte.is_ascii_whitespace()))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

/// Parse `name current -> new` records emitted by `checkupdates` and `paru`.
pub fn parse_update_output(output: &[u8]) -> Vec<PackageUpdate> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let (installed, new_version) = line.trim().split_once(" -> ")?;
            let (name, current_version) = installed.rsplit_once(' ')?;
            Some(PackageUpdate::new(
                name,
                current_version,
                new_version.trim(),
            ))
        })
        .collect()
}

async fn command_output(
    program: &Path,
    args: &[&str],
    deadline: Duration,
) -> Result<Output, UpdatesError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .kill_on_drop(true)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|source| UpdatesError::Spawn {
        program: program.to_path_buf(),
        source,
    })?;
    let process_group = child.id().and_then(|id| i32::try_from(id).ok());
    match timeout(deadline, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(source)) => Err(UpdatesError::Spawn {
            program: program.to_path_buf(),
            source,
        }),
        Err(_) => {
            if let Some(process_group) = process_group {
                let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
            }
            Err(UpdatesError::Timeout {
                program: program.to_path_buf(),
            })
        }
    }
}

/// Perform one official-repository plus optional AUR update check.
///
/// `checkupdates` is required because it refreshes an isolated pacman database;
/// its exit status `2` is the documented no-updates result. A missing or failed
/// `paru` check degrades to the official count rather than hiding valid results.
/// A zero combined count returns `None`, which is the widget's hidden state.
pub async fn check_once(
    checkupdates: &Path,
    paru: &Path,
    deadline: Duration,
) -> Result<Option<PackageUpdates>, UpdatesError> {
    let official_output = command_output(checkupdates, &["--nocolor"], deadline).await?;
    let official = match official_output.status.code() {
        Some(0) => parse_update_output(&official_output.stdout),
        Some(2) => Vec::new(),
        code => {
            return Err(UpdatesError::Failed {
                program: checkupdates.to_path_buf(),
                code,
                stderr: String::from_utf8_lossy(&official_output.stderr).into_owned(),
            });
        }
    };

    let aur = match command_output(paru, &["-Qua"], deadline).await {
        Ok(output) if output.status.success() => parse_update_output(&output.stdout),
        Ok(output)
            if output.status.code() == Some(1)
                && output.stdout.iter().all(u8::is_ascii_whitespace)
                && output.stderr.iter().all(u8::is_ascii_whitespace) =>
        {
            Vec::new()
        }
        Ok(output) => {
            warn!(
                "updates: {} failed with status {}; showing official updates only: {}",
                paru.display(),
                output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_string(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Vec::new()
        }
        Err(UpdatesError::Spawn { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Vec::new()
        }
        Err(error) => {
            warn!("updates: AUR check failed; showing official updates only: {error}");
            Vec::new()
        }
    };

    let updates = PackageUpdates::new(official, aur);
    Ok((updates.total() > 0).then_some(updates))
}

/// Polls Arch repository and AUR updates every five minutes.
pub struct UpdatesProducer {
    interval: Duration,
    command_timeout: Duration,
    checkupdates: PathBuf,
    paru: PathBuf,
}

impl UpdatesProducer {
    /// Create the production update source using commands resolved through PATH.
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            checkupdates: PathBuf::from("checkupdates"),
            paru: PathBuf::from("paru"),
        }
    }

    /// Create a producer with a custom interval for tests or embedding.
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            ..Self::new()
        }
    }
}

impl Default for UpdatesProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for UpdatesProducer {
    fn name(&self) -> String {
        "arch-updates".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(self, tx))
    }
}

async fn run(producer: Box<UpdatesProducer>, tx: MsgSender) -> ProducerResult {
    let mut ticker = interval(producer.interval);
    loop {
        ticker.tick().await;
        let updates = match check_once(
            &producer.checkupdates,
            &producer.paru,
            producer.command_timeout,
        )
        .await
        {
            Ok(updates) => updates,
            Err(error) => {
                warn!("updates: official package check failed: {error}");
                None
            }
        };
        if tx.send(Msg::Updates(updates)).is_err() {
            return Ok(());
        }
    }
}
