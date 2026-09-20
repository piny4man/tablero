//! Opt-in interaction latency diagnostics.
//!
//! Set `TABLERO_PERF=1` and enable the `tablero::performance` log target to
//! emit stable, key-value samples. The normal path does not read the clock.

use std::ffi::OsStr;
use std::fmt;
use std::time::{Duration, Instant};

const ENV_NAME: &str = "TABLERO_PERF";

/// Cheap, cloneable gate and common origin for one process's measurements.
#[derive(Debug, Clone)]
pub(crate) struct PerformanceLogger {
    enabled: bool,
    process_started: Option<Instant>,
}

impl PerformanceLogger {
    pub(crate) fn from_env() -> Self {
        let enabled = enabled_from(std::env::var_os(ENV_NAME).as_deref());
        Self {
            enabled,
            process_started: enabled.then(Instant::now),
        }
    }

    /// Start a sample only when diagnostics are enabled, avoiding clock reads in
    /// the default render and input paths.
    pub(crate) fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    pub(crate) fn record_since(
        &self,
        metric: &'static str,
        started: Option<Instant>,
        fields: fmt::Arguments<'_>,
    ) {
        if let Some(started) = started {
            self.record(metric, started.elapsed(), fields);
        }
    }

    pub(crate) fn record_duration(
        &self,
        metric: &'static str,
        elapsed: Option<Duration>,
        fields: fmt::Arguments<'_>,
    ) {
        if let Some(elapsed) = elapsed {
            self.record(metric, elapsed, fields);
        }
    }

    pub(crate) fn record_process_elapsed(&self, metric: &'static str, fields: fmt::Arguments<'_>) {
        if let Some(started) = self.process_started {
            self.record(metric, started.elapsed(), fields);
        }
    }

    fn record(&self, metric: &'static str, elapsed: Duration, fields: fmt::Arguments<'_>) {
        log::info!(
            target: "tablero::performance",
            "metric={metric} duration_us={} {fields}",
            elapsed_micros(elapsed)
        );
    }
}

fn enabled_from(value: Option<&OsStr>) -> bool {
    value.and_then(OsStr::to_str).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn elapsed_micros(elapsed: Duration) -> u128 {
    let nanos = elapsed.as_nanos();
    if nanos == 0 { 0 } else { nanos.div_ceil(1_000) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::time::Duration;

    #[test]
    fn diagnostics_require_an_explicit_truthy_value() {
        for value in ["1", "true", "yes", "on"] {
            assert!(enabled_from(Some(OsStr::new(value))), "{value}");
        }
        for value in ["", "0", "false", "no", "off", "unexpected"] {
            assert!(!enabled_from(Some(OsStr::new(value))), "{value}");
        }
        assert!(!enabled_from(None));
    }

    #[test]
    fn elapsed_microseconds_never_rounds_a_nonzero_sample_to_zero() {
        assert_eq!(elapsed_micros(Duration::ZERO), 0);
        assert_eq!(elapsed_micros(Duration::from_nanos(1)), 1);
        assert_eq!(elapsed_micros(Duration::from_micros(42)), 42);
    }
}
