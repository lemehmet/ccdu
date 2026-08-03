//! Formatting helpers shared by every frontend, so the CLI and the TUI never disagree about what
//! a number means.

/// Render a byte count the way a human reads it: `1.4 GiB`, `512 B`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render a count with thin separators: `1 234 567`.
pub fn human_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Render a Unix timestamp as `YYYY-MM-DD HH:MM` in UTC.
///
/// Deliberately dependency-free: a date library would be the largest thing in the build for one
/// line of output. Uses Howard Hinnant's civil-from-days algorithm.
pub fn format_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// Same, to the second. Used where two timestamps are compared and minute resolution would print
/// them as identical.
pub fn format_time_secs(secs: i64) -> String {
    let rem = secs.rem_euclid(86_400);
    format!("{}:{:02}", format_time(secs), rem % 60)
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_to_one_decimal() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(15_431_680_000), "14.4 GiB");
        assert_eq!(human_size(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn counts_group_by_threes() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1000), "1 000");
        assert_eq!(human_count(118_737), "118 737");
    }

    #[test]
    fn timestamps_match_known_dates() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_000_000_000), "2001-09-09 01:46");
        // A leap day, to catch the century rules.
        assert_eq!(format_time(951_782_400), "2000-02-29 00:00");
        // Before the epoch, where naive division goes wrong.
        assert_eq!(format_time(-1), "1969-12-31 23:59");
    }

    #[test]
    fn second_resolution_distinguishes_times_within_a_minute() {
        assert_eq!(format_time_secs(0), "1970-01-01 00:00:00");
        assert_eq!(format_time_secs(1_000_000_000), "2001-09-09 01:46:40");
        // The case this exists for: same minute, different instant.
        assert_ne!(format_time_secs(1_000_000_000), format_time_secs(1_000_000_010));
        assert_eq!(format_time(1_000_000_000), format_time(1_000_000_010));
    }
}
