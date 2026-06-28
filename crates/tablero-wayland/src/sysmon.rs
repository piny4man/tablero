//! System-pressure source: CPU and memory load from procfs.
//!
//! Samples the kernel's `/proc/stat` and `/proc/meminfo` on a fixed cadence and
//! emits typed [`Msg::System`] snapshots through the [producer bridge](crate::producer),
//! so system stats reach the render loop the same way every other message does —
//! the rendering code never reads procfs directly.
//!
//! CPU load is a *rate*, not an instantaneous value: `/proc/stat` reports
//! cumulative jiffies spent in each state since boot, so the producer keeps the
//! previous sample and reports the busy fraction *between* samples. Memory is a
//! single read of total vs. available. Both are normalized into a [`SystemStats`]
//! and emitted on each tick. The parsing lives in pure functions
//! ([`parse_cpu_times`], [`cpu_usage`], [`parse_mem_usage`]) the tests drive
//! directly — the full procfs text → message → widget path is covered without
//! sampling the live machine.

use std::fs;
use std::time::Duration;

use log::warn;
use tokio::time::interval;

use tablero_core::widget::{Msg, SystemStats};

use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// How often the producer samples system load.
///
/// Two seconds is frequent enough to track pressure as it moves but far too
/// coarse to keep the loop busy: between ticks the producer is parked on a timer
/// and the render loop is idle, waking only when a sample changes a visible
/// percent.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);

/// A point-in-time reading of the kernel's aggregate CPU time counters.
///
/// Both fields are cumulative jiffies since boot: `total` across every state and
/// `idle` for the idle states (idle + iowait). CPU load is the busy fraction of
/// the change in these between two readings — see [`cpu_usage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    /// Jiffies spent idle (idle + iowait).
    idle: u64,
    /// Jiffies spent in all states.
    total: u64,
}

/// Parse the aggregate `cpu` line of `/proc/stat` into its time counters.
///
/// `/proc/stat` opens with an aggregate `cpu` line (whose first token is exactly
/// `cpu`) followed by per-core `cpu0`, `cpu1`, … lines; only the aggregate is
/// read. The numeric fields are `user nice system idle iowait irq softirq steal
/// guest guest_nice`; `total` sums them and `idle` is `idle + iowait`. Returns
/// `None` if the aggregate line is missing or malformed. Pure over its input.
pub fn parse_cpu_times(proc_stat: &str) -> Option<CpuTimes> {
    let line = proc_stat
        .lines()
        .find(|l| l.split_whitespace().next() == Some("cpu"))?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .map(|t| t.parse::<u64>())
        .collect::<Result<_, _>>()
        .ok()?;
    // Need at least user/nice/system/idle; iowait and the rest are optional on
    // older kernels.
    if nums.len() < 4 {
        return None;
    }
    let total: u64 = nums.iter().sum();
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    Some(CpuTimes { idle, total })
}

/// The busy fraction (0.0–100.0) of CPU time between two readings.
///
/// Load is `busy / total` over the *change* in counters from `prev` to `cur`.
/// A zero or backwards delta (no elapsed time, or a counter reset) degrades to
/// `0.0` rather than dividing by zero or underflowing.
pub fn cpu_usage(prev: CpuTimes, cur: CpuTimes) -> f64 {
    let total = cur.total.saturating_sub(prev.total);
    if total == 0 {
        return 0.0;
    }
    let idle = cur.idle.saturating_sub(prev.idle);
    let busy = total.saturating_sub(idle);
    busy as f64 / total as f64 * 100.0
}

/// Parse `/proc/meminfo` into memory load (0.0–100.0): used as a fraction of total.
///
/// Used memory is `MemTotal - MemAvailable` — `MemAvailable` is the kernel's own
/// estimate of what is reclaimable for new work, the figure a user reads as
/// "free", so this matches what tools like `free` report. Returns `None` if
/// either field is missing or `MemTotal` is zero. Pure over its input.
pub fn parse_mem_usage(proc_meminfo: &str) -> Option<f64> {
    let mut total = None;
    let mut available = None;
    for line in proc_meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = parse_kb(rest);
        }
    }
    let total = total?;
    let available = available?;
    if total == 0 {
        return None;
    }
    let used = total.saturating_sub(available);
    Some(used as f64 / total as f64 * 100.0)
}

/// Parse the leading kibibyte value of a `/proc/meminfo` field (e.g. `  16384 kB`).
fn parse_kb(field: &str) -> Option<u64> {
    field.split_whitespace().next()?.parse().ok()
}

/// Read and parse the current CPU counters, logging and degrading to `None` on
/// any failure so a transient read error never ends the stream.
fn read_cpu_times() -> Option<CpuTimes> {
    // procfs reads are served from kernel memory, not disk, so this does not
    // meaningfully block the runtime.
    match fs::read_to_string("/proc/stat") {
        Ok(stat) => parse_cpu_times(&stat),
        Err(e) => {
            warn!("sysmon: reading /proc/stat failed: {e}");
            None
        }
    }
}

/// Read and parse the current memory load, logging and degrading to `None` on
/// any failure.
fn read_mem_usage() -> Option<f64> {
    match fs::read_to_string("/proc/meminfo") {
        Ok(meminfo) => parse_mem_usage(&meminfo),
        Err(e) => {
            warn!("sysmon: reading /proc/meminfo failed: {e}");
            None
        }
    }
}

/// A [`Producer`] that samples CPU and memory load on a fixed cadence.
///
/// Construct with [`new`](SystemProducer::new) and hand it to the producer
/// bridge; it seeds a CPU baseline, then on every tick reports the load since the
/// previous sample until the render loop shuts down.
pub struct SystemProducer {
    interval: Duration,
}

impl SystemProducer {
    /// Create a system-stats producer sampling at the default cadence.
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
        }
    }

    /// Create a producer sampling at a custom `interval` (used by tests).
    pub fn with_interval(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Default for SystemProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for SystemProducer {
    fn name(&self) -> String {
        "system-stats".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx, self.interval))
    }
}

/// Drive the sampling loop: seed a CPU baseline, then emit load on each tick.
///
/// CPU load needs two readings to form a delta, so the first tick (which a Tokio
/// [`interval`] fires immediately) only seeds the baseline; every tick after that
/// emits a [`SystemStats`]. A failed CPU read degrades that tick's CPU figure to
/// `0` and keeps the last good baseline; a failed memory read degrades to `0`.
/// Returns `Ok(())` once the render loop drops its receiver.
async fn run(tx: MsgSender, period: Duration) -> ProducerResult {
    let mut ticker = interval(period);

    // The first tick completes immediately; use it only to seed the CPU baseline
    // so the first emitted sample reflects real elapsed time.
    ticker.tick().await;
    let mut prev = read_cpu_times();

    loop {
        ticker.tick().await;

        let cur = read_cpu_times();
        let cpu = match (prev, cur) {
            (Some(p), Some(c)) => cpu_usage(p, c),
            // No baseline yet (or this read failed): report 0 this tick.
            _ => 0.0,
        };
        if cur.is_some() {
            prev = cur;
        }

        let mem = read_mem_usage().unwrap_or(0.0);

        if tx.send(Msg::System(SystemStats::new(cpu, mem))).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An aggregate `cpu` line (idle + iowait = 800 + 30 = 830; total = 1000)
    // followed by a per-core line the parser must skip, plus unrelated fields.
    const PROC_STAT: &str = "cpu  100 0 50 800 30 0 20 0 0 0
cpu0 50 0 25 400 15 0 10 0 0 0
intr 12345 0 0
ctxt 67890
";

    const PROC_MEMINFO: &str = "MemTotal:       16000000 kB
MemFree:         2000000 kB
MemAvailable:    8000000 kB
Buffers:          500000 kB
Cached:          3000000 kB
";

    #[test]
    fn parse_cpu_times_reads_the_aggregate_line() {
        let times = parse_cpu_times(PROC_STAT).expect("aggregate cpu line");
        assert_eq!(times.total, 1000);
        assert_eq!(times.idle, 830);
    }

    #[test]
    fn parse_cpu_times_rejects_input_without_an_aggregate_line() {
        // Only per-core lines: no aggregate `cpu` line to read.
        assert_eq!(parse_cpu_times("cpu0 1 2 3 4\nintr 5\n"), None);
        assert_eq!(parse_cpu_times("not proc stat"), None);
    }

    #[test]
    fn parse_cpu_times_rejects_a_truncated_aggregate_line() {
        // Fewer than the four required fields (user/nice/system/idle).
        assert_eq!(parse_cpu_times("cpu 1 2 3\n"), None);
    }

    #[test]
    fn cpu_usage_is_the_busy_fraction_between_samples() {
        // 100 jiffies elapsed, 50 of them idle: 50% busy.
        let prev = CpuTimes {
            idle: 80,
            total: 100,
        };
        let cur = CpuTimes {
            idle: 130,
            total: 200,
        };
        assert!((cpu_usage(prev, cur) - 50.0).abs() < 1e-6);
    }

    #[test]
    fn cpu_usage_handles_no_elapsed_time() {
        let same = CpuTimes {
            idle: 80,
            total: 100,
        };
        assert_eq!(cpu_usage(same, same), 0.0);
    }

    #[test]
    fn cpu_usage_handles_a_counter_reset() {
        // A backwards delta (e.g. after a reset) must not underflow; report 0.
        let prev = CpuTimes {
            idle: 130,
            total: 200,
        };
        let cur = CpuTimes {
            idle: 80,
            total: 100,
        };
        assert_eq!(cpu_usage(prev, cur), 0.0);
    }

    #[test]
    fn parse_mem_usage_is_used_over_total() {
        // 16000000 total, 8000000 available -> 8000000 used -> 50%.
        let mem = parse_mem_usage(PROC_MEMINFO).expect("mem fields present");
        assert!((mem - 50.0).abs() < 1e-6);
    }

    #[test]
    fn parse_mem_usage_requires_both_fields() {
        assert_eq!(parse_mem_usage("MemTotal: 16000000 kB\n"), None);
        assert_eq!(parse_mem_usage("MemAvailable: 8000000 kB\n"), None);
    }

    #[test]
    fn parse_mem_usage_rejects_zero_total() {
        assert_eq!(
            parse_mem_usage("MemTotal: 0 kB\nMemAvailable: 0 kB\n"),
            None
        );
    }
}
