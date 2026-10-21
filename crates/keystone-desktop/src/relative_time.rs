//! Short, local-time labels for how long ago something happened.

/// A moment in unix milliseconds, with the local UTC offset in effect at that moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalTime {
    pub ms: u64,
    pub offset_secs: i64,
}

impl LocalTime {
    /// Reads the local time zone's offset for `ms`.
    pub fn at(ms: u64) -> Self {
        Self {
            ms,
            offset_secs: utc_offset_secs(ms),
        }
    }

    /// Days since 1970-01-01 on the local calendar.
    fn day(self) -> i64 {
        ((self.ms / 1000) as i64 + self.offset_secs).div_euclid(86_400)
    }
}

#[cfg(unix)]
fn utc_offset_secs(ms: u64) -> i64 {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: `localtime_r` writes only to the `tm` passed in and reads `secs`.
    unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            0
        } else {
            tm.tm_gmtoff as i64
        }
    }
}

#[cfg(not(unix))]
fn utc_offset_secs(_: u64) -> i64 {
    0
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// How long before `now` the moment `then` was: `now`, `5m`, `3h`, a weekday within
/// a week, and a date after that. A `then` in the future reads as `now`.
pub fn relative(now: LocalTime, then: LocalTime) -> String {
    let age = now.ms.saturating_sub(then.ms) / 1000;
    match age {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", age / 60),
        3_600..86_400 => format!("{}h", age / 3_600),
        86_400..604_800 => WEEKDAYS[(then.day() + 4).rem_euclid(7) as usize].into(),
        _ => {
            let (month, day) = month_and_day(then.day());
            format!("{} {day}", MONTHS[month - 1])
        }
    }
}

/// When `then`, usually in the future, falls as seen from `now`: `14:05` on the same
/// day, `Fri 14:05` within the next six days, and a date after that or before today.
pub fn clock(now: LocalTime, then: LocalTime) -> String {
    let seconds = ((then.ms / 1000) as i64 + then.offset_secs).rem_euclid(86_400);
    let time = format!("{:02}:{:02}", seconds / 3_600, seconds % 3_600 / 60);
    match then.day() - now.day() {
        0 => time,
        1..7 => format!(
            "{} {time}",
            WEEKDAYS[(then.day() + 4).rem_euclid(7) as usize]
        ),
        _ => {
            let (month, day) = month_and_day(then.day());
            format!("{} {day}", MONTHS[month - 1])
        }
    }
}

/// The month (1-12) and day of the month for a count of days since 1970-01-01.
/// Howard Hinnant's `civil_from_days`.
fn month_and_day(days: i64) -> (usize, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (month as usize, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wednesday 2026-10-14 12:00:00 UTC.
    const NOON: u64 = 1_791_979_200_000;
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    fn utc(ms: u64) -> LocalTime {
        LocalTime { ms, offset_secs: 0 }
    }

    #[test]
    fn clock_times_name_the_day_only_when_it_is_not_today() {
        let now = utc(NOON);
        let at = |offset: u64| clock(now, utc(NOON + offset));
        assert_eq!(at(0), "12:00");
        assert_eq!(at(2 * HOUR + 5 * MINUTE), "14:05");
        assert_eq!(at(12 * HOUR), "Thu 00:00");
        assert_eq!(at(2 * DAY + 3 * HOUR), "Fri 15:00");
        assert_eq!(at(7 * DAY), "Oct 21");
        assert_eq!(clock(now, utc(NOON - DAY)), "Oct 13");
        let east = LocalTime {
            ms: NOON + 11 * HOUR,
            offset_secs: 2 * 3_600,
        };
        assert_eq!(clock(now, east), "Thu 01:00");
    }

    #[test]
    fn labels_follow_the_age_table() {
        let now = utc(NOON);
        let label = |age: u64| relative(now, utc(NOON - age));
        assert_eq!(label(0), "now");
        assert_eq!(label(59_999), "now");
        assert_eq!(label(MINUTE), "1m");
        assert_eq!(label(5 * MINUTE + 30_000), "5m");
        assert_eq!(label(HOUR - 1), "59m");
        assert_eq!(label(HOUR), "1h");
        assert_eq!(label(3 * HOUR), "3h");
        assert_eq!(label(DAY - 1), "23h");
        assert_eq!(label(DAY), "Tue");
        assert_eq!(label(6 * DAY), "Thu");
        assert_eq!(label(7 * DAY), "Oct 7");
        assert_eq!(label(11 * DAY), "Oct 3");
        assert_eq!(label(300 * DAY), "Dec 18");
        assert_eq!(relative(now, utc(NOON + HOUR)), "now");
    }

    #[test]
    fn weekdays_and_dates_use_the_local_calendar() {
        // 23:30 UTC on Saturday 2026-10-03 is already Sunday 2026-10-04 at UTC+2,
        // and still Saturday at UTC-5.
        let then = NOON - 10 * DAY - 12 * HOUR - 30 * MINUTE;
        let at = |offset_secs| LocalTime {
            ms: then,
            offset_secs,
        };
        let now = utc(NOON);
        assert_eq!(relative(now, at(2 * 3_600)), "Oct 4");
        assert_eq!(relative(now, at(-5 * 3_600)), "Oct 3");
        let now = utc(then + 2 * DAY);
        assert_eq!(relative(now, at(2 * 3_600)), "Sun");
        assert_eq!(relative(now, at(-5 * 3_600)), "Sat");
    }
}
