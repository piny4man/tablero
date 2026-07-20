use chrono::{DateTime, Local, Timelike};

/// Format a local timestamp as "HH:MM".
///
/// Pure over its input so widget update logic can be tested deterministically
/// without reaching for the wall clock.
pub fn format_time(now: DateTime<Local>) -> String {
    now.format("%H:%M").to_string()
}

/// Format the current local time as "HH:MM".
pub fn format_clock() -> String {
    format_time(Local::now())
}

/// Milliseconds until the next whole wall-clock minute.
///
/// Used to align the event loop's tick timer to the minute boundary so the clock
/// text flips exactly when the displayed value changes — and the loop stays idle
/// for the rest of the minute instead of waking once a second.
pub fn millis_until_next_minute() -> u64 {
    let now = Local::now();
    millis_to_next_minute(now.second(), now.timestamp_subsec_millis())
}

/// Pure helper: given the seconds elapsed into the current minute and the
/// sub-second component in milliseconds, return how many milliseconds remain
/// until the next whole minute. Always in `1..=60_000` (a value exactly on the
/// boundary waits one full minute). Both inputs are reduced modulo their period
/// so a leap second (second 60, sub-second past 1000) stays in range.
fn millis_to_next_minute(secs: u32, subsec_millis: u32) -> u64 {
    let elapsed = u64::from(secs % 60) * 1000 + u64::from(subsec_millis % 1000);
    60_000 - elapsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn format_time_is_zero_padded_hh_mm() {
        let dt = Local.with_ymd_and_hms(2026, 6, 27, 9, 5, 3).unwrap();
        assert_eq!(format_time(dt), "09:05");
    }

    #[test]
    fn format_clock_returns_hh_mm() {
        let s = format_clock();
        assert_eq!(s.len(), 5);
        assert_eq!(s.chars().nth(2), Some(':'));
        for (i, c) in s.chars().enumerate() {
            if i == 2 {
                continue;
            }
            assert!(c.is_ascii_digit(), "char at index {i} is not a digit");
        }
    }

    #[test]
    fn millis_to_next_minute_is_complement() {
        assert_eq!(millis_to_next_minute(0, 0), 60_000);
        assert_eq!(millis_to_next_minute(0, 1), 59_999);
        assert_eq!(millis_to_next_minute(0, 250), 59_750);
        assert_eq!(millis_to_next_minute(30, 500), 29_500);
        assert_eq!(millis_to_next_minute(59, 0), 1_000);
        assert_eq!(millis_to_next_minute(59, 999), 1);
    }

    #[test]
    fn millis_to_next_minute_handles_leap_overflow() {
        // A leap second can report second 60 and a sub-second count past 1000;
        // both wrap into range rather than under/overflowing.
        assert_eq!(millis_to_next_minute(60, 1500), 59_500);
    }

    #[test]
    fn millis_until_next_minute_in_range() {
        let ms = millis_until_next_minute();
        assert!((1..=60_000).contains(&ms), "out of range: {ms}");
    }
}
