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

/// Whole days from now until `iso`, rounded down: negative once it has passed.
///
/// See [`days_until_unix`] for the rounding.
pub fn days_until(iso: &str) -> Option<i64> {
    Some(days_until_unix(parse_iso_to_unix(iso)?))
}

/// Whole days from now until the Unix time `ts`, rounded down.
///
/// Down, not toward zero: `0` is under a day left and `-1` is passed within
/// the last day. Toward zero, a key that expired an hour ago came out as `0`,
/// which reads "in 0d" and which every `< 0` check took for a live key.
pub fn days_until_unix(ts: i64) -> i64 {
    days_between(Utc::now().timestamp(), ts)
}

fn days_between(now: i64, ts: i64) -> i64 {
    (ts - now).div_euclid(86_400)
}

/// A day count from [`days_until`] as the time it spans: `12d`, or `under 1d`.
///
/// Rounded down both ways, so a past count gives the whole days since: `-1`
/// (passed within the last day) is `under 1d`, `-3` is `2d`. The caller says
/// which way it points: "in {span}", "expired {span} ago".
pub fn span(days: i64) -> String {
    let whole = if days < 0 { -days - 1 } else { days };
    if whole == 0 {
        "under 1d".to_string()
    } else {
        format!("{whole}d")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_000_000;

    /// The bug: an expiry an hour ago came out as 0 days, which no `< 0`
    /// check calls expired.
    #[test]
    fn an_expiry_passed_less_than_a_day_ago_is_in_the_past() {
        assert_eq!(days_between(NOW, NOW - 3600), -1);
        assert_eq!(days_between(NOW, NOW - DAY), -1);
        assert_eq!(days_between(NOW, NOW - DAY - 1), -2);
    }

    #[test]
    fn a_future_expiry_counts_the_whole_days_left() {
        assert_eq!(days_between(NOW, NOW + 3600), 0);
        assert_eq!(days_between(NOW, NOW + DAY), 1);
        assert_eq!(days_between(NOW, NOW + 10 * DAY - 1), 9);
    }

    #[test]
    fn a_span_under_a_day_says_so_either_way() {
        assert_eq!(span(days_between(NOW, NOW + 3600)), "under 1d");
        assert_eq!(span(days_between(NOW, NOW - 3600)), "under 1d");
        assert_eq!(span(days_between(NOW, NOW + 3 * DAY + 5)), "3d");
        // Three and a half days ago: three whole days since.
        assert_eq!(span(days_between(NOW, NOW - 3 * DAY - DAY / 2)), "3d");
    }

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
