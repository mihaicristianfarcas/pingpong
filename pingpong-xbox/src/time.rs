//! Wall-clock time as the services write it: tokens expire at an RFC 3339
//! UTC time (`"2026-10-07T03:38:41.9483157Z"`) or after a number of seconds.
//! Seconds since the Unix epoch are kept, so a stored token's expiry
//! survives a restart.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds since the epoch of an RFC 3339 time in UTC (`Z`), fractions of a
/// second dropped; `None` for anything else.
pub fn parse_utc(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let secs = days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(secs).ok()
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xsts_times_parse_to_unix_seconds() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(
            parse_utc("2026-10-07T03:38:41.9483157Z"),
            Some(1_791_344_321)
        );
    }

    #[test]
    fn other_shapes_are_refused() {
        assert_eq!(parse_utc("2026-10-07T03:38:41+02:00"), None);
        assert_eq!(parse_utc("2026-13-07T03:38:41Z"), None);
        assert_eq!(parse_utc("yesterday"), None);
        assert_eq!(parse_utc("1969-12-31T23:59:59Z"), None);
    }
}
