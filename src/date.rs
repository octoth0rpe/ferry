//! UTC calendar arithmetic. ferry writes every date in UTC, so this is all
//! the date handling it needs.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    secs(SystemTime::now())
}

/// Seconds since the Unix epoch, negative before it.
pub fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs_f64().ceil() as i64),
    }
}

/// `2026-10-16T03:00:00.123Z`
pub fn timestamp(secs: i64, millis: u32) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `2026-10-16`
pub fn date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to (year, month, day), from Howard Hinnant's
/// `civil_from_days`: <https://howardhinnant.github.io/date_algorithms.html>
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// (year, month, day) to days since 1970-01-01, the inverse of
/// [`civil_from_days`]. `m` is 1–12 and `d` is 1–31.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * if m > 2 { m - 3 } else { m + 9 } + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(timestamp(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(date(-1), "1969-12-31");
    }

    #[test]
    fn leap_years() {
        let d = days_from_civil(2024, 2, 29);
        assert_eq!(civil_from_days(d), (2024, 2, 29));
        assert_eq!(civil_from_days(d + 1), (2024, 3, 1));
        // 2100 isn't a leap year; 2000 is.
        let d = days_from_civil(2100, 2, 28);
        assert_eq!(civil_from_days(d + 1), (2100, 3, 1));
        let d = days_from_civil(2000, 2, 28);
        assert_eq!(civil_from_days(d + 1), (2000, 2, 29));
    }

    #[test]
    fn round_trips() {
        for days in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn formats() {
        // 2026-10-16T03:00:00Z
        let t = days_from_civil(2026, 10, 16) * 86_400 + 3 * 3600;
        assert_eq!(timestamp(t, 42), "2026-10-16T03:00:00.042Z");
        assert_eq!(timestamp(t + 86_399, 999), "2026-10-17T02:59:59.999Z");
        assert_eq!(date(t), "2026-10-16");
    }

    #[test]
    fn system_time() {
        use std::time::Duration;
        assert_eq!(secs(UNIX_EPOCH + Duration::from_millis(1500)), 1);
        assert_eq!(secs(UNIX_EPOCH - Duration::from_millis(1500)), -2);
    }
}
