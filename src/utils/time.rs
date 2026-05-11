//! Shared date/time utilities.
//!
//! Provides ISO timestamp formatting and parsing used by skill usage telemetry
//! and the curator lifecycle manager. Extracted from duplicated code in
//! `usage.rs` and `curator.rs`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Return the current UTC time as an ISO-8601 string: `"2025-07-23T10:30:00Z"`.
pub fn now_iso() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;
    let (year, month, day) = days_to_date(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since UNIX epoch to (year, month, day).
/// Uses Howard Hinnant's algorithm.
pub fn days_to_date(days: u64) -> (u64, u64, u64) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Convert (year, month, day) to days since UNIX epoch.
pub fn date_to_days(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let m = if month <= 2 { month + 9 } else { month - 3 };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parse an ISO-8601 timestamp (`"2025-07-23T10:30:00Z"`) to `SystemTime`.
pub fn parse_iso_to_systemtime(ts: &str) -> Option<SystemTime> {
    let ts = ts.strip_suffix('Z').unwrap_or(ts);
    let parts: Vec<&str> = ts.split(['T', ':', '-']).collect();
    if parts.len() < 6 {
        return None;
    }
    let year: u64 = parts[0].parse().ok()?;
    let month: u64 = parts[1].parse().ok()?;
    let day: u64 = parts[2].parse().ok()?;
    let hour: u64 = parts[3].parse().ok()?;
    let min: u64 = parts[4].parse().ok()?;
    let sec: u64 = parts[5].parse().ok()?;
    let days = date_to_days(year, month, day);
    let total_secs = days * 86400 + hour * 3600 + min * 60 + sec;
    UNIX_EPOCH.checked_add(Duration::from_secs(total_secs))
}

/// Parse an ISO-8601 timestamp to seconds since UNIX epoch.
pub fn parse_iso_to_secs(ts: &str) -> Option<u64> {
    let st = parse_iso_to_systemtime(ts)?;
    st.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// Hours since the given ISO timestamp. Returns `None` if parsing fails.
pub fn hours_since(ts: &str) -> Option<f64> {
    let then_secs = parse_iso_to_secs(ts)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now < then_secs {
        return Some(0.0);
    }
    Some((now - then_secs) as f64 / 3600.0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_now_iso_format() {
        let ts = now_iso();
        assert!(ts.len() >= 20); // "2025-07-23T10:30:00Z"
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn test_days_to_date_roundtrip() {
        let days = date_to_days(2025, 7, 23);
        let (y, m, d) = days_to_date(days);
        assert_eq!(y, 2025);
        assert_eq!(m, 7);
        assert_eq!(d, 23);
    }

    #[test]
    fn test_parse_iso() {
        let st = parse_iso_to_systemtime("2025-07-23T10:30:00Z").unwrap();
        let duration = st.duration_since(UNIX_EPOCH).unwrap();
        assert!(duration.as_secs() > 0);
    }

    #[test]
    fn test_hours_since_now() {
        let ts = now_iso();
        let h = hours_since(&ts).unwrap();
        assert!(h < 0.01);
    }

    #[test]
    fn test_parse_iso_to_secs() {
        let secs = parse_iso_to_secs("2025-07-23T10:30:00Z").unwrap();
        assert!(secs > 0);
    }
}
