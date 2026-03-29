//! Lightweight ISO-8601 parser and formatter for temporal types.
//!
//! No external dependencies (chrono/time crates) -- suitable for wasm32-unknown-unknown.

/// Parse "YYYY-MM-DD" -> days since 1970-01-01.
pub fn parse_date(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i32 = s[..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    ymd_to_days(y, m, d)
}

/// Parse "HH:MM:SS[.nanos]" -> nanoseconds since midnight.
pub fn parse_time(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 8 || b[2] != b':' || b[5] != b':' {
        return None;
    }
    let h: u64 = s[..2].parse().ok()?;
    let min: u64 = s[3..5].parse().ok()?;
    let sec: u64 = s[6..8].parse().ok()?;
    if h >= 24 || min >= 60 || sec >= 60 {
        return None;
    }
    let mut nanos = (h * 3600 + min * 60 + sec) * 1_000_000_000;
    if b.len() > 8 && b[8] == b'.' {
        let frac = &s[9..];
        if frac.is_empty() || frac.len() > 9 {
            return None;
        }
        let mut frac_val: u64 = frac.parse().ok()?;
        for _ in 0..(9 - frac.len()) {
            frac_val *= 10;
        }
        nanos += frac_val;
    }
    Some(nanos)
}

/// Parse "YYYY-MM-DDTHH:MM:SS[.frac][Z|+HH:MM|-HH:MM]" -> (unix_seconds, subsec_nanos).
///
/// Returns UTC-normalized seconds. For local datetimes (no timezone suffix),
/// the result is treated as UTC.
pub fn parse_datetime(s: &str) -> Option<(i64, u32)> {
    if s.len() < 19 {
        return None;
    }
    let date_part = &s[..10];
    if s.as_bytes()[10] != b'T' && s.as_bytes()[10] != b't' {
        return None;
    }
    let days = parse_date(date_part)?;

    let rest = &s[11..];
    let (time_str, tz_str) = split_time_tz(rest)?;
    let time_nanos = parse_time(time_str)?;

    let offset_secs = parse_tz_offset(tz_str)?;

    let total_secs = (days as i64) * 86400 + (time_nanos / 1_000_000_000) as i64 - offset_secs;
    let subsec = (time_nanos % 1_000_000_000) as u32;
    Some((total_secs, subsec))
}

/// Parse "YYYY-MM-DDTHH:MM:SS[.frac]" as a local datetime (no timezone conversion).
/// Returns (unix_seconds_as_if_utc, subsec_nanos).
pub fn parse_local_datetime(s: &str) -> Option<(i64, u32)> {
    if s.len() < 19 {
        return None;
    }
    let date_part = &s[..10];
    if s.as_bytes()[10] != b'T' && s.as_bytes()[10] != b't' {
        return None;
    }
    let days = parse_date(date_part)?;
    let time_str = &s[11..];
    let time_nanos = parse_time(time_str)?;

    let total_secs = (days as i64) * 86400 + (time_nanos / 1_000_000_000) as i64;
    let subsec = (time_nanos % 1_000_000_000) as u32;
    Some((total_secs, subsec))
}

/// Parse "YYYY-MM-DDTHH:MM:SS[.frac][+HH:MM|-HH:MM|Z]" as a zoned datetime.
/// Returns (unix_seconds_utc, subsec_nanos, tz_offset_seconds).
pub fn parse_zoned_datetime(s: &str) -> Option<(i64, u32, i32)> {
    if s.len() < 19 {
        return None;
    }
    let date_part = &s[..10];
    if s.as_bytes()[10] != b'T' && s.as_bytes()[10] != b't' {
        return None;
    }
    let days = parse_date(date_part)?;

    let rest = &s[11..];
    let (time_str, tz_str) = split_time_tz(rest)?;
    let time_nanos = parse_time(time_str)?;
    let offset_secs = parse_tz_offset(tz_str)?;

    let total_secs = (days as i64) * 86400 + (time_nanos / 1_000_000_000) as i64 - offset_secs;
    let subsec = (time_nanos % 1_000_000_000) as u32;
    Some((total_secs, subsec, offset_secs as i32))
}

/// Parse "HH:MM:SS[.frac][+HH:MM|-HH:MM|Z]" as a zoned time.
/// Returns (nanos_since_midnight_local, tz_offset_seconds).
pub fn parse_zoned_time(s: &str) -> Option<(u64, i32)> {
    let (time_str, tz_str) = split_time_tz(s)?;
    let nanos = parse_time(time_str)?;
    let offset_secs = parse_tz_offset(tz_str)?;
    Some((nanos, offset_secs as i32))
}

/// Parse ISO-8601 duration "P[nY][nM][nD][T[nH][nM][nS]]" -> (months, nanos).
pub fn parse_duration(s: &str) -> Option<(i32, i64)> {
    let b = s.as_bytes();
    if b.is_empty() || b[0] != b'P' {
        return None;
    }
    let mut i = 1;
    let mut months: i32 = 0;
    let mut nanos: i64 = 0;
    let mut in_time = false;

    while i < b.len() {
        if b[i] == b'T' {
            in_time = true;
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        if i == start || i >= b.len() {
            return None;
        }
        let num_str = &s[start..i];
        let designator = b[i];
        i += 1;

        if !in_time {
            match designator {
                b'Y' => {
                    let v: i32 = num_str.parse().ok()?;
                    months += v * 12;
                }
                b'M' => {
                    let v: i32 = num_str.parse().ok()?;
                    months += v;
                }
                b'D' => {
                    let v: f64 = num_str.parse().ok()?;
                    nanos += (v * 86_400_000_000_000.0) as i64;
                }
                _ => return None,
            }
        } else {
            match designator {
                b'H' => {
                    let v: f64 = num_str.parse().ok()?;
                    nanos += (v * 3_600_000_000_000.0) as i64;
                }
                b'M' => {
                    let v: f64 = num_str.parse().ok()?;
                    nanos += (v * 60_000_000_000.0) as i64;
                }
                b'S' => {
                    let v: f64 = num_str.parse().ok()?;
                    nanos += (v * 1_000_000_000.0) as i64;
                }
                _ => return None,
            }
        }
    }
    Some((months, nanos))
}

// ---- Formatting ----

/// Format days since epoch -> "YYYY-MM-DD".
pub fn format_date(days: i32) -> String {
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Format nanos since midnight -> "HH:MM:SS[.nnnnnnnnn]".
pub fn format_time(nanos: u64) -> String {
    let total_secs = nanos / 1_000_000_000;
    let sub = nanos % 1_000_000_000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if sub == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        let frac = format!("{sub:09}");
        let trimmed = frac.trim_end_matches('0');
        format!("{h:02}:{m:02}:{s:02}.{trimmed}")
    }
}

/// Format (unix_seconds, subsec_nanos) -> "YYYY-MM-DDTHH:MM:SSZ" or with fractional seconds.
pub fn format_datetime(secs: i64, sub: u32) -> String {
    let day_secs = secs.rem_euclid(86400);
    let days = ((secs - day_secs) / 86400) as i32;
    let date = format_date(days);
    let time = format_time(day_secs as u64 * 1_000_000_000 + sub as u64);
    format!("{date}T{time}Z")
}

/// Format (unix_seconds, subsec_nanos) -> "YYYY-MM-DDTHH:MM:SS" (no timezone suffix).
pub fn format_local_datetime(secs: i64, sub: u32) -> String {
    let day_secs = secs.rem_euclid(86400);
    let days = ((secs - day_secs) / 86400) as i32;
    let date = format_date(days);
    let time = format_time(day_secs as u64 * 1_000_000_000 + sub as u64);
    format!("{date}T{time}")
}

/// Format (months, nanos) -> ISO-8601 duration string.
pub fn format_duration(months: i32, nanos: i64) -> String {
    let mut out = String::from("P");
    let (abs_months, neg_months) = if months < 0 {
        (-months as u32, true)
    } else {
        (months as u32, false)
    };
    let years = abs_months / 12;
    let rem_months = abs_months % 12;
    if years > 0 {
        if neg_months {
            out.push_str(&format!("-{years}Y"));
        } else {
            out.push_str(&format!("{years}Y"));
        }
    }
    if rem_months > 0 {
        if neg_months {
            out.push_str(&format!("-{rem_months}M"));
        } else {
            out.push_str(&format!("{rem_months}M"));
        }
    }

    let (abs_nanos, neg_nanos) = if nanos < 0 {
        (-nanos as u64, true)
    } else {
        (nanos as u64, false)
    };
    let total_secs = abs_nanos / 1_000_000_000;
    let sub_nanos = abs_nanos % 1_000_000_000;
    let days = total_secs / 86400;
    let rem_secs = total_secs % 86400;
    let hours = rem_secs / 3600;
    let minutes = (rem_secs % 3600) / 60;
    let seconds = rem_secs % 60;

    if days > 0 {
        if neg_nanos {
            out.push_str(&format!("-{days}D"));
        } else {
            out.push_str(&format!("{days}D"));
        }
    }

    if hours > 0 || minutes > 0 || seconds > 0 || sub_nanos > 0 {
        out.push('T');
        if hours > 0 {
            if neg_nanos {
                out.push_str(&format!("-{hours}H"));
            } else {
                out.push_str(&format!("{hours}H"));
            }
        }
        if minutes > 0 {
            if neg_nanos {
                out.push_str(&format!("-{minutes}M"));
            } else {
                out.push_str(&format!("{minutes}M"));
            }
        }
        if seconds > 0 || sub_nanos > 0 {
            let prefix = if neg_nanos { "-" } else { "" };
            if sub_nanos > 0 {
                let frac = format!("{sub_nanos:09}");
                let trimmed = frac.trim_end_matches('0');
                out.push_str(&format!("{prefix}{seconds}.{trimmed}S"));
            } else {
                out.push_str(&format!("{prefix}{seconds}S"));
            }
        }
    }

    if out == "P" {
        out.push_str("0D");
    }
    out
}

// ---- Helpers ----

pub fn is_leap_year(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Convert (year, month, day) -> days since 1970-01-01.
pub fn ymd_to_days(y: i32, m: u32, d: u32) -> Option<i32> {
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    let y = y as i64;
    let m = m as i64;
    let d = d as i64;
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days as i32)
}

/// Convert days since 1970-01-01 -> (year, month, day).
pub fn days_to_ymd(days: i32) -> (i32, u32, u32) {
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

// ---- Internal helpers ----

/// Split time string from timezone suffix.
fn split_time_tz(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    if b.len() < 8 {
        return None;
    }
    let mut i = 8;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    Some((&s[..i], &s[i..]))
}

/// Parse timezone offset: "" -> 0, "Z" -> 0, "+HH:MM" -> secs, "-HH:MM" -> -secs.
fn parse_tz_offset(s: &str) -> Option<i64> {
    if s.is_empty() || s == "Z" || s == "z" {
        return Some(0);
    }
    let b = s.as_bytes();
    let sign: i64 = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    if rest.len() < 5 || rest.as_bytes()[2] != b':' {
        return None;
    }
    let h: i64 = rest[..2].parse().ok()?;
    let m: i64 = rest[3..5].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_roundtrip() {
        assert_eq!(ymd_to_days(1970, 1, 1), Some(0));
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn known_dates() {
        assert_eq!(ymd_to_days(2024, 3, 15), Some(19797));
        let (y, m, d) = days_to_ymd(19797);
        assert_eq!((y, m, d), (2024, 3, 15));
    }

    #[test]
    fn leap_year_feb29() {
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2023));
        assert_eq!(days_in_month(2024, 2), 29);
        assert!(ymd_to_days(2024, 2, 29).is_some());
        assert!(ymd_to_days(2023, 2, 29).is_none());
    }

    #[test]
    fn parse_date_basic() {
        assert_eq!(parse_date("2024-03-15"), Some(19797));
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert!(parse_date("bad").is_none());
        assert!(parse_date("2024-13-01").is_none());
    }

    #[test]
    fn parse_time_basic() {
        assert_eq!(parse_time("00:00:00"), Some(0));
        assert_eq!(parse_time("01:00:00"), Some(3_600_000_000_000));
        assert_eq!(parse_time("14:30:00"), Some(52_200_000_000_000));
        assert_eq!(parse_time("14:30:00.5"), Some(52_200_500_000_000));
        assert_eq!(parse_time("14:30:00.123456789"), Some(52_200_123_456_789));
    }

    #[test]
    fn parse_datetime_utc() {
        let (secs, sub) = parse_datetime("2024-03-15T14:30:00Z").unwrap();
        assert_eq!(sub, 0);
        assert_eq!(secs, 19797 * 86400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_datetime_offset() {
        let (secs, _) = parse_datetime("2024-03-15T14:30:00+09:00").unwrap();
        let (secs_utc, _) = parse_datetime("2024-03-15T14:30:00Z").unwrap();
        assert_eq!(secs, secs_utc - 9 * 3600);
    }

    #[test]
    fn parse_zoned_datetime_preserves_offset() {
        let (secs, sub, tz) = parse_zoned_datetime("2024-03-15T14:30:00+09:00").unwrap();
        assert_eq!(tz, 9 * 3600);
        assert_eq!(sub, 0);
        // UTC time should be 05:30
        let (secs_utc, _) = parse_datetime("2024-03-15T05:30:00Z").unwrap();
        assert_eq!(secs, secs_utc);
    }

    #[test]
    fn parse_zoned_time_basic() {
        let (nanos, tz) = parse_zoned_time("14:30:00+09:00").unwrap();
        assert_eq!(nanos, 52_200_000_000_000);
        assert_eq!(tz, 9 * 3600);
    }

    #[test]
    fn parse_local_datetime_basic() {
        let (secs, sub) = parse_local_datetime("2024-03-15T14:30:00").unwrap();
        assert_eq!(sub, 0);
        assert_eq!(secs, 19797 * 86400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_duration_basic() {
        assert_eq!(
            parse_duration("P1Y2M3DT4H5M6S"),
            Some((
                14,
                3 * 86_400_000_000_000 + 4 * 3_600_000_000_000 + 5 * 60_000_000_000 + 6_000_000_000
            ))
        );
        assert_eq!(
            parse_duration("PT1H30M"),
            Some((0, 3_600_000_000_000 + 30 * 60_000_000_000))
        );
        assert_eq!(parse_duration("P30D"), Some((0, 30 * 86_400_000_000_000)));
        assert_eq!(parse_duration("P1M"), Some((1, 0)));
    }

    #[test]
    fn format_date_basic() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(19797), "2024-03-15");
    }

    #[test]
    fn format_time_basic() {
        assert_eq!(format_time(0), "00:00:00");
        assert_eq!(format_time(52_200_000_000_000), "14:30:00");
        assert_eq!(format_time(52_200_500_000_000), "14:30:00.5");
    }

    #[test]
    fn format_datetime_basic() {
        let secs = 19797i64 * 86400 + 14 * 3600 + 30 * 60;
        assert_eq!(format_datetime(secs, 0), "2024-03-15T14:30:00Z");
    }

    #[test]
    fn format_local_datetime_basic() {
        let secs = 19797i64 * 86400 + 14 * 3600 + 30 * 60;
        assert_eq!(format_local_datetime(secs, 0), "2024-03-15T14:30:00");
    }

    #[test]
    fn format_duration_basic() {
        assert_eq!(format_duration(0, 0), "P0D");
        assert_eq!(format_duration(14, 0), "P1Y2M");
        assert_eq!(format_duration(0, 3_600_000_000_000), "PT1H");
        assert_eq!(
            format_duration(1, 86_400_000_000_000 + 3_600_000_000_000),
            "P1M1DT1H"
        );
    }

    #[test]
    fn negative_epoch_date() {
        assert_eq!(ymd_to_days(1969, 12, 31), Some(-1));
        assert_eq!(days_to_ymd(-1), (1969, 12, 31));
    }

    // ---- Additional coverage tests ----

    #[test]
    fn parse_date_error_paths() {
        // Too short
        assert!(parse_date("2024-03").is_none());
        // Wrong separators
        assert!(parse_date("2024/03/15").is_none());
        assert!(parse_date("2024-03/15").is_none());
        // Invalid day 0
        assert!(parse_date("2024-03-00").is_none());
        // Day exceeds month
        assert!(parse_date("2024-04-31").is_none());
        // Month 0
        assert!(parse_date("2024-00-15").is_none());
        // Non-numeric
        assert!(parse_date("YYYY-MM-DD").is_none());
    }

    #[test]
    fn parse_time_error_paths() {
        // Too short
        assert!(parse_time("14:30").is_none());
        // Wrong separators
        assert!(parse_time("14-30-00").is_none());
        assert!(parse_time("14:30-00").is_none());
        // Hour >= 24
        assert!(parse_time("24:00:00").is_none());
        // Minute >= 60
        assert!(parse_time("14:60:00").is_none());
        // Second >= 60
        assert!(parse_time("14:30:60").is_none());
        // Empty fractional part
        assert!(parse_time("14:30:00.").is_none());
        // Fractional part too long (>9 digits)
        assert!(parse_time("14:30:00.1234567890").is_none());
        // Non-numeric fractional part
        assert!(parse_time("14:30:00.abc").is_none());
    }

    #[test]
    fn parse_time_fractional_padding() {
        // 3-digit frac should be padded to 9 digits
        assert_eq!(parse_time("14:30:00.123"), Some(52_200_123_000_000));
        // 1-digit frac
        assert_eq!(parse_time("14:30:00.1"), Some(52_200_100_000_000));
        // 9-digit frac (no padding needed)
        assert_eq!(parse_time("14:30:00.000000001"), Some(52_200_000_000_001));
    }

    #[test]
    fn parse_datetime_error_paths() {
        // Too short
        assert!(parse_datetime("2024-03-15").is_none());
        // No T separator
        assert!(parse_datetime("2024-03-15 14:30:00Z").is_none());
        // Invalid date component
        assert!(parse_datetime("2024-13-15T14:30:00Z").is_none());
        // Invalid time component
        assert!(parse_datetime("2024-03-15T25:30:00Z").is_none());
    }

    #[test]
    fn parse_datetime_lowercase_t() {
        let (secs, _) = parse_datetime("2024-03-15t14:30:00Z").unwrap();
        assert_eq!(secs, 19797 * 86400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_datetime_no_tz_suffix() {
        // Empty timezone treated as UTC
        let (secs, _) = parse_datetime("2024-03-15T14:30:00").unwrap();
        assert_eq!(secs, 19797 * 86400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_datetime_negative_offset() {
        let (secs, _) = parse_datetime("2024-03-15T14:30:00-05:00").unwrap();
        let (secs_utc, _) = parse_datetime("2024-03-15T14:30:00Z").unwrap();
        assert_eq!(secs, secs_utc + 5 * 3600);
    }

    #[test]
    fn parse_datetime_with_frac_and_tz() {
        let (secs, sub) = parse_datetime("2024-03-15T14:30:00.500Z").unwrap();
        assert_eq!(sub, 500_000_000);
        assert_eq!(secs, 19797 * 86400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_local_datetime_error_paths() {
        // Too short
        assert!(parse_local_datetime("2024-03-15").is_none());
        // No T separator
        assert!(parse_local_datetime("2024-03-15 14:30:00").is_none());
        // Invalid date
        assert!(parse_local_datetime("2024-13-15T14:30:00").is_none());
        // Invalid time
        assert!(parse_local_datetime("2024-03-15T25:30:00").is_none());
    }

    #[test]
    fn parse_local_datetime_lowercase_t() {
        let (secs, _) = parse_local_datetime("2024-03-15t14:30:00").unwrap();
        assert_eq!(secs, 19797 * 86400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_local_datetime_with_frac() {
        let (secs, sub) = parse_local_datetime("2024-03-15T14:30:00.250").unwrap();
        assert_eq!(sub, 250_000_000);
        assert_eq!(secs, 19797 * 86400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn parse_zoned_datetime_error_paths() {
        // Too short
        assert!(parse_zoned_datetime("2024-03-15").is_none());
        // No T separator
        assert!(parse_zoned_datetime("2024-03-15 14:30:00Z").is_none());
        // Invalid date
        assert!(parse_zoned_datetime("2024-13-15T14:30:00Z").is_none());
        // Invalid time
        assert!(parse_zoned_datetime("2024-03-15T25:30:00Z").is_none());
        // Invalid tz
        assert!(parse_zoned_datetime("2024-03-15T14:30:00X").is_none());
    }

    #[test]
    fn parse_zoned_datetime_lowercase_t() {
        let (secs, sub, tz) = parse_zoned_datetime("2024-03-15t14:30:00+09:00").unwrap();
        assert_eq!(tz, 9 * 3600);
        assert_eq!(sub, 0);
        let (secs_utc, _) = parse_datetime("2024-03-15T05:30:00Z").unwrap();
        assert_eq!(secs, secs_utc);
    }

    #[test]
    fn parse_zoned_datetime_negative_offset() {
        let (_, _, tz) = parse_zoned_datetime("2024-03-15T14:30:00-05:00").unwrap();
        assert_eq!(tz, -5 * 3600);
    }

    #[test]
    fn parse_zoned_datetime_with_frac() {
        let (_, sub, tz) = parse_zoned_datetime("2024-03-15T14:30:00.750+09:00").unwrap();
        assert_eq!(sub, 750_000_000);
        assert_eq!(tz, 9 * 3600);
    }

    #[test]
    fn parse_zoned_time_error_paths() {
        // Too short
        assert!(parse_zoned_time("14:30").is_none());
        // Invalid time
        assert!(parse_zoned_time("25:30:00Z").is_none());
    }

    #[test]
    fn parse_zoned_time_with_z() {
        let (nanos, tz) = parse_zoned_time("14:30:00Z").unwrap();
        assert_eq!(nanos, 52_200_000_000_000);
        assert_eq!(tz, 0);
    }

    #[test]
    fn parse_zoned_time_lowercase_z() {
        let (nanos, tz) = parse_zoned_time("14:30:00z").unwrap();
        assert_eq!(nanos, 52_200_000_000_000);
        assert_eq!(tz, 0);
    }

    #[test]
    fn parse_zoned_time_negative_offset() {
        let (nanos, tz) = parse_zoned_time("14:30:00-05:30").unwrap();
        assert_eq!(nanos, 52_200_000_000_000);
        assert_eq!(tz, -(5 * 3600 + 30 * 60));
    }

    #[test]
    fn parse_zoned_time_with_frac() {
        let (nanos, tz) = parse_zoned_time("14:30:00.5+09:00").unwrap();
        assert_eq!(nanos, 52_200_500_000_000);
        assert_eq!(tz, 9 * 3600);
    }

    #[test]
    fn parse_duration_error_paths() {
        // Empty string
        assert!(parse_duration("").is_none());
        // No P prefix
        assert!(parse_duration("1Y").is_none());
        // Just P (no components — returns Some since loop ends)
        // Actually "P" alone: loop ends immediately with months=0, nanos=0
        // Unknown designator in date part
        assert!(parse_duration("P1X").is_none());
        // Unknown designator in time part
        assert!(parse_duration("PT1X").is_none());
        // Trailing number without designator
        assert!(parse_duration("P1Y2").is_none());
        // T at end with nothing after
        assert!(parse_duration("PT").is_some()); // no components parsed = (0,0)
        // Just P
        assert!(parse_duration("P").is_some()); // (0, 0)
    }

    #[test]
    fn parse_duration_fractional_seconds() {
        let (months, nanos) = parse_duration("PT1.5S").unwrap();
        assert_eq!(months, 0);
        assert_eq!(nanos, 1_500_000_000);
    }

    #[test]
    fn parse_duration_fractional_hours_and_minutes() {
        let (_, nanos) = parse_duration("PT1.5H").unwrap();
        assert_eq!(nanos, (1.5 * 3_600_000_000_000.0) as i64);

        let (_, nanos) = parse_duration("PT1.5M").unwrap();
        assert_eq!(nanos, (1.5 * 60_000_000_000.0) as i64);
    }

    #[test]
    fn parse_duration_only_years() {
        assert_eq!(parse_duration("P2Y"), Some((24, 0)));
    }

    #[test]
    fn parse_duration_only_time_seconds() {
        assert_eq!(parse_duration("PT30S"), Some((0, 30_000_000_000)));
    }

    #[test]
    fn format_duration_negative_months() {
        assert_eq!(format_duration(-14, 0), "P-1Y-2M");
        assert_eq!(format_duration(-3, 0), "P-3M");
    }

    #[test]
    fn format_duration_negative_nanos() {
        // Negative time: -1 hour 30 minutes 15 seconds
        let nanos = -(3_600_000_000_000i64 + 30 * 60_000_000_000 + 15_000_000_000);
        let s = format_duration(0, nanos);
        assert!(s.contains("-1H"));
        assert!(s.contains("-30M"));
        assert!(s.contains("-15S"));
    }

    #[test]
    fn format_duration_negative_days() {
        let nanos = -(2 * 86_400_000_000_000i64);
        assert_eq!(format_duration(0, nanos), "P-2D");
    }

    #[test]
    fn format_duration_sub_nanoseconds() {
        // Duration with only sub-second nanos
        assert_eq!(format_duration(0, 500_000_000), "PT0.5S");
        assert_eq!(format_duration(0, 1_000_000), "PT0.001S");
    }

    #[test]
    fn format_duration_negative_sub_nanoseconds() {
        assert_eq!(format_duration(0, -500_000_000), "PT-0.5S");
    }

    #[test]
    fn format_duration_minutes_only() {
        assert_eq!(format_duration(0, 120_000_000_000), "PT2M");
    }

    #[test]
    fn format_duration_seconds_and_sub() {
        // 2.5 seconds
        assert_eq!(format_duration(0, 2_500_000_000), "PT2.5S");
    }

    #[test]
    fn format_datetime_with_subsec() {
        let secs = 19797i64 * 86400 + 14 * 3600 + 30 * 60;
        assert_eq!(format_datetime(secs, 500_000_000), "2024-03-15T14:30:00.5Z");
    }

    #[test]
    fn format_datetime_negative_seconds() {
        // Before epoch
        let s = format_datetime(-86400, 0);
        assert_eq!(s, "1969-12-31T00:00:00Z");
    }

    #[test]
    fn format_local_datetime_with_subsec() {
        let secs = 19797i64 * 86400 + 14 * 3600 + 30 * 60;
        assert_eq!(
            format_local_datetime(secs, 500_000_000),
            "2024-03-15T14:30:00.5"
        );
    }

    #[test]
    fn days_in_month_all_months() {
        assert_eq!(days_in_month(2024, 1), 31);
        assert_eq!(days_in_month(2024, 3), 31);
        assert_eq!(days_in_month(2024, 4), 30);
        assert_eq!(days_in_month(2024, 5), 31);
        assert_eq!(days_in_month(2024, 6), 30);
        assert_eq!(days_in_month(2024, 7), 31);
        assert_eq!(days_in_month(2024, 8), 31);
        assert_eq!(days_in_month(2024, 9), 30);
        assert_eq!(days_in_month(2024, 10), 31);
        assert_eq!(days_in_month(2024, 11), 30);
        assert_eq!(days_in_month(2024, 12), 31);
        // Invalid month
        assert_eq!(days_in_month(2024, 0), 0);
        assert_eq!(days_in_month(2024, 13), 0);
    }

    #[test]
    fn leap_year_century_rules() {
        // Divisible by 100 but not 400 => not leap
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2100));
        // Divisible by 400 => leap
        assert!(is_leap_year(2000));
        assert!(is_leap_year(1600));
    }

    #[test]
    fn ymd_to_days_invalid() {
        // Month out of range
        assert!(ymd_to_days(2024, 0, 1).is_none());
        assert!(ymd_to_days(2024, 13, 1).is_none());
        // Day 0
        assert!(ymd_to_days(2024, 1, 0).is_none());
        // Day exceeds month length
        assert!(ymd_to_days(2024, 2, 30).is_none());
        assert!(ymd_to_days(2023, 2, 29).is_none());
    }

    #[test]
    fn days_to_ymd_far_past() {
        // Test a very negative day value
        let (y, m, d) = days_to_ymd(-719468);
        assert_eq!((y, m, d), (0, 3, 1)); // March 1, year 0
    }

    #[test]
    fn parse_tz_offset_edge_cases() {
        // Lowercase z
        assert_eq!(parse_tz_offset("z"), Some(0));
        // Empty
        assert_eq!(parse_tz_offset(""), Some(0));
        // Invalid character
        assert_eq!(parse_tz_offset("X"), None);
        // Too short after sign
        assert_eq!(parse_tz_offset("+01"), None);
        // Missing colon
        assert_eq!(parse_tz_offset("+0100"), None);
    }

    #[test]
    fn split_time_tz_with_frac_and_tz() {
        let (time, tz) = split_time_tz("14:30:00.5+09:00").unwrap();
        assert_eq!(time, "14:30:00.5");
        assert_eq!(tz, "+09:00");
    }

    #[test]
    fn split_time_tz_no_frac() {
        let (time, tz) = split_time_tz("14:30:00Z").unwrap();
        assert_eq!(time, "14:30:00");
        assert_eq!(tz, "Z");
    }

    #[test]
    fn split_time_tz_too_short() {
        assert!(split_time_tz("14:30").is_none());
    }
}
