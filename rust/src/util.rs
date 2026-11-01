//! Small helpers: timestamped logging and UTC date arithmetic (no date crate needed).

use std::time::{SystemTime, UNIX_EPOCH};

pub fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// (year, month, day) in UTC for a unix timestamp. Howard Hinnant's civil_from_days.
pub fn civil(t: i64) -> (i64, u32, u32) {
    let z = t.div_euclid(86400) + 719_468;
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

/// `YYYY-MM-DD` in UTC. History files are named by this, so lexical order is date order.
pub fn ymd(t: i64) -> String {
    let (y, m, d) = civil(t);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn log(msg: &str) {
    let s = unix_now().rem_euclid(86400);
    eprintln!("{:02}:{:02}:{:02} {msg}", s / 3600, s % 3600 / 60, s % 60);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ymd_matches_known_dates() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(951_782_400), "2000-02-29");
        assert_eq!(ymd(1_793_493_300), "2026-11-01");
        assert_eq!(ymd(1_793_491_199), "2026-10-31");
        assert_eq!(ymd(-1), "1969-12-31");
    }
}
