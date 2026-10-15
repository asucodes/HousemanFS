//! Display formatting for byte counts and ages.
//!
//! Three frontends (CLI, finder, desktop) report the same numbers, so they spell them the
//! same way. Formatting lives here — pure string handling with no I/O — so the spellings
//! cannot drift apart between interfaces.

/// A byte count in binary units, labelled honestly.
///
/// Windows shows binary units but calls them GB; labelling them GiB costs nothing and
/// avoids repeating that confusion.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// A byte count with its exact value alongside, for reports where precision matters.
pub fn human_bytes_exact(bytes: u64) -> String {
    format!("{} ({bytes} bytes)", human_bytes(bytes))
}

/// Ticks per second in Windows time.
pub const TICKS_PER_SECOND: i64 = 10_000_000;

/// Parse `500MB` into bytes. Units are binary (MB = 1024²), matching how the tool reports
/// sizes everywhere else; a bare number means bytes. Stated once so the CLI's ask parser
/// and the desktop's size fields cannot disagree about what a gigabyte is.
pub fn parse_byte_size(amount: &str) -> Option<u64> {
    let lower = amount.to_ascii_lowercase();
    let (digits, factor) = if let Some(n) = lower.strip_suffix("tb") {
        (n, 1u64 << 40)
    } else if let Some(n) = lower.strip_suffix("gb") {
        (n, 1u64 << 30)
    } else if let Some(n) = lower.strip_suffix("mb") {
        (n, 1u64 << 20)
    } else if let Some(n) = lower.strip_suffix("kb") {
        (n, 1u64 << 10)
    } else if let Some(n) = lower.strip_suffix('b') {
        (n, 1)
    } else {
        (lower.as_str(), 1)
    };
    digits.parse::<u64>().ok()?.checked_mul(factor)
}

/// Current time in Windows ticks (100ns since 1601-01-01), at full precision.
///
/// Truncating to whole seconds would leave the clock up to a second behind a file written
/// "now", and every fresh file would read as modified in the future.
pub fn windows_ticks_now() -> i64 {
    const UNIX_TO_WINDOWS_TICKS: u128 = 11_644_473_600 * 10_000_000;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100 + UNIX_TO_WINDOWS_TICKS) as i64)
        .unwrap_or(0)
}

/// A Windows timestamp as an age. 0 (or negative) means unknown and is shown as unknown —
/// "a long time ago" would be a guess wearing a measurement's clothes. A timestamp ahead
/// of `now` is a broken clock, reported as one rather than as a negative age.
pub fn human_age(modified: i64, now: i64) -> String {
    if modified <= 0 {
        return "unknown".to_string();
    }
    if modified > now {
        return "in the future — the clock moved, not the file".to_string();
    }
    let seconds = (now - modified) / TICKS_PER_SECOND;
    const MINUTE: i64 = 60;
    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    let (value, unit) = if seconds < MINUTE {
        (seconds, "second")
    } else if seconds < HOUR {
        (seconds / MINUTE, "minute")
    } else if seconds < DAY {
        (seconds / HOUR, "hour")
    } else if seconds < MONTH {
        (seconds / DAY, "day")
    } else if seconds < YEAR {
        (seconds / MONTH, "month")
    } else {
        (seconds / YEAR, "year")
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(human_bytes_exact(1024), "1.00 KiB (1024 bytes)");
    }

    #[test]
    fn ages_read_as_ages() {
        assert_eq!(human_age(0, 100), "unknown");
        assert_eq!(human_age(-5, 100), "unknown");
        let now = windows_ticks_now();
        assert!(human_age(now - 30 * TICKS_PER_SECOND, now).starts_with("30 second"));
        assert!(human_age(now - 3 * 86_400 * TICKS_PER_SECOND, now).starts_with("3 day"));
        assert!(human_age(now + 10, now).starts_with("in the future"));
    }

    #[test]
    fn the_clock_keeps_sub_second_precision() {
        // Two consecutive reads must not go backwards, and the clock must resolve below
        // one second — truncation once made every fresh file read as future-dated.
        let first = windows_ticks_now();
        let second = windows_ticks_now();
        assert!(second >= first);
        assert!(second % TICKS_PER_SECOND != 0 || first % TICKS_PER_SECOND != 0);
    }
}
