use chrono::{DateTime, Local};

/// Format a local timestamp as "HH:MM:SS".
///
/// Pure over its input so widget update logic can be tested deterministically
/// without reaching for the wall clock.
pub fn format_time(now: DateTime<Local>) -> String {
    now.format("%H:%M:%S").to_string()
}

/// Format the current local time as "HH:MM:SS".
pub fn format_clock() -> String {
    format_time(Local::now())
}

/// Milliseconds until the next whole wall-clock second.
///
/// Used to align the event loop's tick timer to the second boundary so the
/// clock text flips exactly when the displayed value changes.
pub fn millis_until_next_second() -> u64 {
    millis_to_next_second(Local::now().timestamp_subsec_millis())
}

/// Pure helper: given the sub-second component in milliseconds, return how many
/// milliseconds remain until the next whole second. Always in `1..=1000`.
fn millis_to_next_second(subsec_millis: u32) -> u64 {
    let ms = subsec_millis % 1000;
    if ms == 0 { 1000 } else { u64::from(1000 - ms) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn format_time_is_zero_padded_hh_mm_ss() {
        let dt = Local.with_ymd_and_hms(2026, 6, 27, 9, 5, 3).unwrap();
        assert_eq!(format_time(dt), "09:05:03");
    }

    #[test]
    fn format_clock_returns_hh_mm_ss() {
        let s = format_clock();
        assert_eq!(s.len(), 8);
        assert_eq!(s.chars().nth(2), Some(':'));
        assert_eq!(s.chars().nth(5), Some(':'));
        for (i, c) in s.chars().enumerate() {
            if i == 2 || i == 5 {
                continue;
            }
            assert!(c.is_ascii_digit(), "char at index {i} is not a digit");
        }
    }

    #[test]
    fn millis_to_next_second_is_complement() {
        assert_eq!(millis_to_next_second(0), 1000);
        assert_eq!(millis_to_next_second(1), 999);
        assert_eq!(millis_to_next_second(250), 750);
        assert_eq!(millis_to_next_second(999), 1);
    }

    #[test]
    fn millis_to_next_second_handles_leap_overflow() {
        // Leap seconds can push the sub-second count past 1000.
        assert_eq!(millis_to_next_second(1500), 500);
    }

    #[test]
    fn millis_until_next_second_in_range() {
        let ms = millis_until_next_second();
        assert!((1..=1000).contains(&ms), "out of range: {ms}");
    }
}
