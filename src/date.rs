//! UTC calendar-day math for the usage/cost views (v0.4 M6). Both
//! `/_ccm/cost?day=` and `ccm history cost` bucket usage records by UTC
//! calendar day, which needs civil-from-days, its inverse, and a strict
//! `YYYY-MM-DD` parser; all three live here so the endpoint and the CLI
//! agree by construction. Howard Hinnant's algorithms, no dependencies,
//! presentation only — filtering and ordering always use raw
//! `timestamp_ms` numbers.

/// Wall-clock unix ms (same definition as the other `now_ms` helpers; this
/// one exists so day-picker call sites read as date logic, not clock access).
pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `(year, month, day, hour, minute, second)` of a unix-ms timestamp, UTC.
pub(crate) fn utc_parts(ms: u64) -> (i64, u64, u64, u64, u64, u64) {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    (year, month, day, rem / 3_600, (rem % 3_600) / 60, rem % 60)
}

/// `YYYY-MM-DD` of `ms` in UTC.
pub(crate) fn utc_day_of(ms: u64) -> String {
    let (year, month, day, ..) = utc_parts(ms);
    format!("{year:04}-{month:02}-{day:02}")
}

/// UTC `YYYY-MM-DDTHH:MM:SSZ` for a unix-ms timestamp. The shared
/// presentation helper for every view that prints an instant (history CLI
/// tables, advise reports, `ccm clients`) — presentation only, filtering
/// and ordering always use the raw `timestamp_ms` numbers.
pub(crate) fn utc_instant(ms: u64) -> String {
    let (year, month, day, hour, minute, second) = utc_parts(ms);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Parse `YYYY-MM-DD` into that UTC day's midnight, unix ms. `None` on any
/// malformed input — wrong shape, non-numeric fields, or an impossible date
/// (month 13, April 31, February 30). Leap years are honored via a
/// days-from-civil → civil-from-days roundtrip: a real date survives the
/// roundtrip, an impossible one does not.
pub(crate) fn parse_utc_day(text: &str) -> Option<u64> {
    let (year, month, day) = parse_ymd(text)?;
    let days = days_from_civil(year, month, day);
    if civil_from_days(days) != (year, month, day) {
        return None; // e.g. 2023-02-30 normalizes to March 2
    }
    // days_from_civil already returns days since the epoch. Pre-1970 dates
    // (year 0001 through 1969 — real civil dates that survive the roundtrip
    // above) have negative day counts and cannot be a u64 timestamp;
    // try_from rejects them instead of wrapping, and checked_mul keeps the
    // conversion panic/overflow-free even for adversarial year values.
    let days = u64::try_from(days).ok()?;
    days.checked_mul(86_400_000)
}

/// Split `YYYY-MM-DD` into numbers, without validating the ranges.
fn parse_ymd(text: &str) -> Option<(i64, u64, u64)> {
    let (year, rest) = text.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    if year.len() != 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    Some((year.parse().ok()?, month.parse().ok()?, day.parse().ok()?))
}

/// Days since 1970-01-01 for a civil date (Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400; // [0, 399]
    let mp = if month > 2 {
        month as i64 - 3
    } else {
        month as i64 + 9
    };
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01 (Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    // The algorithm's month (1-12) and day (1-31) are always positive.
    (year, month as u64, day as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_of_matches_parts() {
        assert_eq!(utc_day_of(0), "1970-01-01");
        // 2025-10-03T00:00:00Z = 1_759_449_600 s
        assert_eq!(utc_day_of(1_759_449_600_000), "2025-10-03");
        // 23:59:59.999 UTC still belongs to the same day
        assert_eq!(utc_day_of(1_759_535_999_999), "2025-10-03");
        // ... and the next millisecond rolls over
        assert_eq!(utc_day_of(1_759_536_000_000), "2025-10-04");
        // 2026-10-03T00:00:00Z = 1_790_985_600 s
        assert_eq!(utc_day_of(1_790_985_600_000), "2026-10-03");
    }

    #[test]
    fn parse_roundtrips_real_days() {
        for text in [
            "1970-01-01",
            "2000-02-29",
            "2026-10-03",
            "2024-12-31",
            "2100-02-28",
        ] {
            let start = parse_utc_day(text).unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(utc_day_of(start), text, "{text} survives the roundtrip");
        }
    }

    #[test]
    fn parse_rejects_pre_epoch_days() {
        // Real civil dates before 1970-01-01 (they survive the calendar
        // roundtrip) have no u64 unix-ms representation and must be None —
        // a bare `as u64` cast would wrap them into huge timestamps
        // (M6 verify finding: release builds then served a garbage day
        // window instead of a 400).
        for text in ["1969-12-31", "0000-01-01", "0001-01-01"] {
            assert_eq!(parse_utc_day(text), None, "{text} predates the epoch");
        }
    }

    #[test]
    fn parse_rejects_malformed_and_impossible_days() {
        // impossible calendar dates
        for text in [
            "2023-02-30",
            "2023-13-01",
            "2023-04-31",
            "2023-00-10",
            "2023-01-00",
            "2100-02-29", // 2100 is not a leap year
        ] {
            assert_eq!(parse_utc_day(text), None, "{text} must not parse");
        }
        // wrong shapes
        for text in [
            "2026-1-3",
            "26-10-03",
            "2026/10/03",
            "2026-10-03T00",
            "2026-10",
            "",
            "abcd-ef-gh",
        ] {
            assert_eq!(parse_utc_day(text), None, "{text:?} must not parse");
        }
    }

    #[test]
    fn parts_match_known_instants() {
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, 0));
        // leap day 2000-02-29T00:00:00Z = 951_782_400 s
        assert_eq!(utc_parts(951_782_400_000), (2000, 2, 29, 0, 0, 0));
        // 12:34:56 later the same leap day
        assert_eq!(utc_parts(951_827_696_000), (2000, 2, 29, 12, 34, 56));
    }

    #[test]
    fn instant_formats_known_instants() {
        assert_eq!(utc_instant(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_instant(951_782_400_000), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(utc_instant(1_000_000_000_000), "2001-09-09T01:46:40Z");
    }
}
