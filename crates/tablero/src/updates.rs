//! Arch Linux package-update source.
//!
//! Each check runs `anzen updates --refresh --json` when anzen is installed.
//! `--refresh` syncs anzen's private copy of the sync databases, so the live
//! databases are never touched and no partial upgrade is possible. The JSON
//! report (schema 1) carries the repository, AUR and vendor lists and a status
//! per source; any source that is not `ok` or `skipped` becomes a problem
//! line, so a failed check is never shown as zero updates.
//!
//! Without anzen on `PATH`, the check falls back to `checkupdates`, which
//! also syncs a private database copy, plus `paru -Qua` for the AUR when paru
//! is installed. A failed AUR check is a problem line, not zero updates. The
//! source is chosen on every check, so installing or removing anzen takes
//! effect without a restart. Any anzen failure other than "not found" stays a
//! failed check; it never silently switches source.
//!
//! Every command runs directly, without a shell, bounded by a timeout that
//! kills its whole process group. Tablero only reads; upgrading stays in the
//! package manager.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use log::{info, warn};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{interval, timeout};

use crate::widget::{Msg, PackageUpdate, PackageUpdates, UpdatesStatus};

use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

const DEFAULT_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// The only `anzen updates --json` format this reader understands.
const SCHEMA: u32 = 1;
/// The arguments of every anzen check.
pub const ANZEN_ARGS: [&str; 3] = ["updates", "--refresh", "--json"];
/// The arguments of the fallback repository check.
pub const CHECKUPDATES_ARGS: [&str; 1] = ["--nocolor"];
/// The arguments of the fallback AUR check.
pub const PARU_ARGS: [&str; 1] = ["-Qua"];

/// A failure that leaves no trustworthy report.
#[derive(Debug)]
pub enum UpdatesError {
    /// A command could not be started.
    Spawn { program: PathBuf, source: io::Error },
    /// A command exceeded its execution deadline.
    Timeout { program: PathBuf },
    /// A command exited with a status that means there is no report: anything
    /// but `0` for anzen `--json`, anything but `0` or `2` for `checkupdates`.
    Failed {
        program: PathBuf,
        code: Option<i32>,
        stderr: String,
    },
    /// The output was not a valid report.
    Parse(serde_json::Error),
    /// The report uses a format version this reader does not know.
    Schema(u32),
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
            Self::Parse(error) => write!(f, "unreadable anzen report: {error}"),
            Self::Schema(schema) => write!(
                f,
                "anzen report schema {schema} is not supported (expected {SCHEMA}); update tablero"
            ),
        }
    }
}

impl UpdatesError {
    /// Whether the program does not exist, as opposed to failing once run.
    fn is_not_found(&self) -> bool {
        matches!(self, Self::Spawn { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }
}

impl Error for UpdatesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Timeout { .. } | Self::Failed { .. } | Self::Schema(_) => None,
        }
    }
}

/// `anzen updates --json`, schema 1. Unknown fields are ignored: new ones can
/// appear without a schema bump.
#[derive(Debug, Deserialize)]
struct Report {
    checked_at: Option<u64>,
    sources: Sources,
}

#[derive(Debug, Deserialize)]
struct Sources {
    repo: RepoSource,
    aur: AurSource,
    vendor: VendorSource,
}

#[derive(Debug, Deserialize)]
struct RepoSource {
    status: Status,
    synced_at: Option<u64>,
    stale: Option<bool>,
    #[serde(default)]
    updates: Vec<ReportUpdate>,
    error: Option<String>,
    refresh_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AurSource {
    status: Status,
    #[serde(default)]
    updates: Vec<ReportUpdate>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VendorSource {
    status: Status,
    #[serde(default)]
    updates: Vec<ReportUpdate>,
    #[serde(default)]
    failed: Vec<FailedItem>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReportUpdate {
    name: String,
    old: String,
    new: String,
}

#[derive(Debug, Deserialize)]
struct FailedItem {
    name: String,
    error: String,
}

/// A source's status. An unknown value is kept as a problem rather than
/// rejected, so a new status degrades the widget instead of breaking it.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Partial,
    Unavailable,
    Failed,
    Skipped,
    #[serde(other)]
    Unknown,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Unknown => "in an unknown state",
        }
    }

    fn is_problem(&self) -> bool {
        !matches!(self, Self::Ok | Self::Skipped)
    }
}

fn package_updates(updates: Vec<ReportUpdate>) -> Vec<PackageUpdate> {
    updates
        .into_iter()
        .map(|update| PackageUpdate::new(update.name, update.old, update.new))
        .collect()
}

/// `"AUR unavailable: <error>"`, or just the status when anzen gave no reason.
fn source_problem(source: &str, status: &Status, error: Option<String>) -> String {
    match error {
        Some(error) => format!("{source} {}: {error}", status.label()),
        None => format!("{source} {}", status.label()),
    }
}

/// Parse one `anzen updates --json` report.
///
/// The schema is checked before anything else, so a future format is reported
/// as such rather than as a confusing field error.
pub fn parse_report(output: &[u8]) -> Result<PackageUpdates, UpdatesError> {
    #[derive(Deserialize)]
    struct Version {
        schema: u32,
    }
    let version: Version = serde_json::from_slice(output).map_err(UpdatesError::Parse)?;
    if version.schema != SCHEMA {
        return Err(UpdatesError::Schema(version.schema));
    }
    let report: Report = serde_json::from_slice(output).map_err(UpdatesError::Parse)?;
    let Sources { repo, aur, vendor } = report.sources;

    let mut problems = Vec::new();
    if repo.status.is_problem() {
        problems.push(source_problem("Repositories", &repo.status, repo.error));
    }
    if let Some(error) = repo.refresh_error {
        problems.push(format!(
            "Refresh failed, showing the last system sync: {error}"
        ));
    }
    if aur.status.is_problem() {
        problems.push(source_problem("AUR", &aur.status, aur.error));
    }
    if vendor.status.is_problem() && (vendor.error.is_some() || vendor.failed.is_empty()) {
        problems.push(source_problem("Vendor", &vendor.status, vendor.error));
    }
    problems.extend(
        vendor
            .failed
            .into_iter()
            .map(|item| format!("{}: {}", item.name, item.error)),
    );

    let stale_for = match (repo.stale, repo.synced_at, report.checked_at) {
        (Some(true), Some(synced), Some(checked)) => {
            Some(Duration::from_secs(checked.saturating_sub(synced)))
        }
        // Stale without timestamps: still flagged, with no age to show.
        (Some(true), _, _) => Some(Duration::ZERO),
        _ => None,
    };

    Ok(
        PackageUpdates::new(package_updates(repo.updates), package_updates(aur.updates))
            .with_vendor(package_updates(vendor.updates))
            .with_problems(problems)
            .with_stale_for(stale_for),
    )
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
        .stdin(Stdio::null())
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

/// Run one `anzen updates --refresh --json` check.
#[hotpath::measure]
pub async fn check_once(anzen: &Path, deadline: Duration) -> Result<PackageUpdates, UpdatesError> {
    let output = command_output(anzen, &ANZEN_ARGS, deadline).await?;
    if !output.status.success() {
        return Err(UpdatesError::Failed {
            program: anzen.to_path_buf(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    parse_report(&output.stdout)
}

/// Parse `name current -> new` records, as printed by `checkupdates` and
/// `paru -Qua`. Lines in any other shape are skipped.
pub fn parse_update_lines(output: &[u8]) -> Vec<PackageUpdate> {
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

fn status_label(output: &Output) -> String {
    output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string())
}

/// Run one fallback check: `checkupdates` for the repositories, then
/// `paru -Qua` for the AUR.
///
/// `checkupdates` is required; its status `2` means no updates. paru is
/// optional: when it is not installed the AUR is not checked. When it is
/// installed but fails, the repository list is kept and the failure becomes
/// a problem line, so the widget shows the count as incomplete.
#[hotpath::measure]
pub async fn check_fallback(
    checkupdates: &Path,
    paru: &Path,
    deadline: Duration,
) -> Result<PackageUpdates, UpdatesError> {
    let output = command_output(checkupdates, &CHECKUPDATES_ARGS, deadline).await?;
    let official = match output.status.code() {
        Some(0) => parse_update_lines(&output.stdout),
        Some(2) => Vec::new(),
        code => {
            return Err(UpdatesError::Failed {
                program: checkupdates.to_path_buf(),
                code,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
    };

    let mut problems = Vec::new();
    let aur = match command_output(paru, &PARU_ARGS, deadline).await {
        Ok(output) if output.status.success() => parse_update_lines(&output.stdout),
        // paru exits 1 with no output when nothing is pending.
        Ok(output)
            if output.status.code() == Some(1)
                && output.stdout.trim_ascii().is_empty()
                && output.stderr.trim_ascii().is_empty() =>
        {
            Vec::new()
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut problem = format!(
                "AUR failed: paru exited with status {}",
                status_label(&output)
            );
            if !stderr.trim().is_empty() {
                problem.push_str(": ");
                problem.push_str(stderr.trim());
            }
            problems.push(problem);
            Vec::new()
        }
        Err(error) if error.is_not_found() => Vec::new(),
        Err(error) => {
            problems.push(format!("AUR failed: {error}"));
            Vec::new()
        }
    };

    Ok(PackageUpdates::new(official, aur).with_problems(problems))
}

/// Which program a check used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Anzen,
    Fallback,
}

/// Polls for package updates every five minutes: anzen when installed,
/// otherwise `checkupdates` and paru.
pub struct UpdatesProducer {
    interval: Duration,
    command_timeout: Duration,
    anzen: PathBuf,
    checkupdates: PathBuf,
    paru: PathBuf,
}

impl UpdatesProducer {
    /// Create the production update source, resolving every program through
    /// PATH.
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            anzen: PathBuf::from("anzen"),
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

    /// Use another anzen executable, for tests or a non-PATH install.
    pub fn with_program(mut self, anzen: impl Into<PathBuf>) -> Self {
        self.anzen = anzen.into();
        self
    }

    /// Use other fallback executables, for tests or a non-PATH install.
    pub fn with_fallback_programs(
        mut self,
        checkupdates: impl Into<PathBuf>,
        paru: impl Into<PathBuf>,
    ) -> Self {
        self.checkupdates = checkupdates.into();
        self.paru = paru.into();
        self
    }

    /// Bound each command by `deadline` instead of the default minute.
    pub fn with_command_timeout(mut self, deadline: Duration) -> Self {
        self.command_timeout = deadline;
        self
    }

    /// Run one check: anzen, or the fallback when anzen is not installed.
    pub async fn check(&self) -> Result<PackageUpdates, UpdatesError> {
        self.check_source().await.1
    }

    async fn check_source(&self) -> (Source, Result<PackageUpdates, UpdatesError>) {
        match check_once(&self.anzen, self.command_timeout).await {
            Err(error) if error.is_not_found() => (
                Source::Fallback,
                check_fallback(&self.checkupdates, &self.paru, self.command_timeout).await,
            ),
            result => (Source::Anzen, result),
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
    let mut last_source = None;
    loop {
        ticker.tick().await;
        let (source, result) = producer.check_source().await;
        if last_source != Some(source) {
            match source {
                Source::Anzen => info!("updates: checking with {}", producer.anzen.display()),
                Source::Fallback => info!(
                    "updates: {} not found, checking with {} and {}",
                    producer.anzen.display(),
                    producer.checkupdates.display(),
                    producer.paru.display()
                ),
            }
            last_source = Some(source);
        }
        let status = match result {
            Ok(updates) => UpdatesStatus::Checked(updates),
            Err(error) => {
                warn!("updates: {error}");
                UpdatesStatus::Failed(error.to_string())
            }
        };
        if tx.send(Msg::Updates(status)).is_err() {
            return Ok(());
        }
    }
}
