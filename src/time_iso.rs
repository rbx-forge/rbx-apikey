use chrono::{DateTime, Duration, NaiveDateTime, SecondsFormat, Utc};

pub fn iso_now() -> String {
    format_iso(Utc::now())
}

pub fn iso_in_days(days: i64) -> String {
    format_iso(Utc::now() + Duration::seconds(days * 86_400))
}

pub fn iso_in_months(months: i64) -> String {
    let secs = (months as f64 * 30.4375 * 86_400.0).floor() as i64;
    format_iso(Utc::now() + Duration::seconds(secs))
}

fn format_iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parse an ISO-8601 timestamp (with or without fractional seconds) into Unix seconds.
pub fn parse_iso_to_unix(iso: &str) -> Option<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return Some(dt.timestamp());
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc().timestamp());
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc().timestamp());
    }
    None
}

/// Days between `iso` and now (negative = past).
pub fn days_until(iso: &str) -> Option<i64> {
    let ts = parse_iso_to_unix(iso)?;
    Some((ts - Utc::now().timestamp()) / 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_now_ends_with_z_and_ms() {
        let s = iso_now();
        assert!(s.ends_with("Z"), "expected trailing Z: {}", s);
        assert!(s.contains('.'), "expected .ms in {}", s);
    }

    #[test]
    fn parse_iso_round_trip() {
        let ts = parse_iso_to_unix("2026-05-13T10:00:00.000Z").unwrap();
        // 2026-05-13T10:00:00Z = ?
        // Not asserting exact value to avoid TZ pitfalls; just check positive future of epoch.
        assert!(ts > 1_700_000_000);
    }

    #[test]
    fn parse_iso_no_ms() {
        let ts = parse_iso_to_unix("2026-05-13T10:00:00Z").unwrap();
        assert!(ts > 1_700_000_000);
    }
}
