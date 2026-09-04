//! Turning a unix timestamp into something a person can read, and back.
//!
//! The activity log exists so a user can open it in a text editor and see what
//! happened. `1788480000` does not serve that; a date does. Nothing else in
//! the workspace formats a calendar date — the library stores unix seconds and
//! the Home page counts backwards in days — so the conversion lives here.
//!
//! This is deliberately not a date-and-time library. It converts between unix
//! seconds and `YYYY-MM-DDTHH:MM:SSZ`, and that is the whole of it.
//!
//! **Times are UTC.** Local time is what a reader actually wants, but the
//! offset is platform work and a wrong offset is worse than an honest `Z`.
//! When the log grows a viewer inside the app, that is where local time
//! belongs; the file on disk stays unambiguous.

/// Seconds in a day.
const DAY: i64 = 86_400;

/// The current time, in seconds since the unix epoch.
///
/// A clock set before 1970 yields a negative number rather than an error.
/// Every function here handles that, and an implausible timestamp in a log is
/// more useful than a missing entry.
pub fn now_unix() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

/// Format `unix_seconds` as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format(unix_seconds: i64) -> String {
    // Euclidean rather than truncating division: for a pre-epoch timestamp,
    // `-1 / DAY` is 0 and `-1 % DAY` is -1, which would place the entry on the
    // wrong day and at a negative hour.
    let days = unix_seconds.div_euclid(DAY);
    let seconds = unix_seconds.rem_euclid(DAY);

    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (seconds / 3600, (seconds / 60) % 60, seconds % 60);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Read back a timestamp written by [`format`].
///
/// Returns `None` for anything else. The log is the only caller, and a line it
/// cannot read is a line it skips: there is nothing to be gained by guessing
/// at a corrupted entry.
pub fn parse(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;

    let mut date = date.splitn(3, '-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;

    let mut time = time.splitn(3, ':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    Some(days_from_civil(year, month, day) * DAY + hour * 3600 + minute * 60 + second)
}

// ---------------------------------------------------------------------------
// Civil calendar
// ---------------------------------------------------------------------------
//
// Howard Hinnant's algorithm pair, which is the usual way to do this without a
// lookup table. Both are exact across the whole proleptic Gregorian calendar,
// and they are inverses of each other — which is the property the round-trip
// test below actually checks.
//
// The trick in both is shifting the year to begin on 1 March, so the leap day
// falls at the end of it and every other month length follows one linear rule.
// That is why January and February are counted against the previous year.

/// Days since the epoch to a calendar date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Move the origin to 0000-03-01, which is 719468 days before 1970-01-01.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;

    // Day of the 400-year era, 0..=146096.
    let day_of_era = (shifted - era * 146_097) as u64;

    // Year of the era, 0..=399.
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;

    let year = year_of_era as i64 + era * 400;

    // Day of the March-based year, 0..=365.
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);

    // Month of the March-based year, 0..=11.
    let march_month = (5 * day_of_year + 2) / 153;

    let day = (day_of_year - (153 * march_month + 2) / 5 + 1) as u32;
    let month = if march_month < 10 {
        march_month + 3
    } else {
        march_month - 9
    } as u32;

    (year + i64::from(month <= 2), month, day)
}

/// A calendar date to days since the epoch.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;

    let year_of_era = (year - era * 400) as u64;
    let march_month = u64::from(if month > 2 { month - 3 } else { month + 9 });
    let day_of_year = (153 * march_month + 2) / 5 + u64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;

    era * 146_097 + day_of_era as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_epoch_is_the_epoch() {
        assert_eq!(format(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_time_of_day_is_split_correctly() {
        assert_eq!(format(3661), "1970-01-01T01:01:01Z");
        assert_eq!(format(DAY - 1), "1970-01-01T23:59:59Z");
        assert_eq!(format(DAY), "1970-01-02T00:00:00Z");
    }

    /// The century rule is the part that gets missed: 2000 is a leap year
    /// because it divides by 400, and 1900 is not because it divides by 100.
    #[test]
    fn leap_days_land_where_they_should() {
        assert_eq!(format(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format(1_709_164_800), "2024-02-29T00:00:00Z");

        // The day before 1900-03-01 would be 1900-02-29 under a naive rule.
        let march_1900 = days_from_civil(1900, 3, 1) * DAY;
        assert_eq!(format(march_1900 - DAY), "1900-02-28T00:00:00Z");
    }

    #[test]
    fn a_pre_epoch_time_does_not_run_backwards() {
        assert_eq!(format(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format(-DAY), "1969-12-31T00:00:00Z");
    }

    #[test]
    fn a_year_boundary_is_crossed_cleanly() {
        let new_year = days_from_civil(2026, 1, 1) * DAY;
        assert_eq!(format(new_year), "2026-01-01T00:00:00Z");
        assert_eq!(format(new_year - 1), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn parsing_undoes_formatting() {
        assert_eq!(parse("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse("1969-12-31T23:59:59Z"), Some(-1));
    }

    /// The two halves being inverses is the only property the log depends on.
    /// Stepping by an awkward number of seconds walks across every month
    /// length, both leap rules, and the epoch itself.
    #[test]
    fn every_timestamp_survives_a_round_trip() {
        let mut at = -2_000_000_000;
        while at < 4_000_000_000 {
            let text = format(at);
            assert_eq!(parse(&text), Some(at), "{text} did not round-trip");
            at += 999_983;
        }
    }

    #[test]
    fn rubbish_is_rejected_rather_than_guessed_at() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("2026-09-04"), None, "no time part");
        assert_eq!(parse("2026-09-04T12:00:00"), None, "no zone marker");
        assert_eq!(parse("2026-13-04T00:00:00Z"), None, "month 13");
        assert_eq!(parse("2026-09-32T00:00:00Z"), None, "day 32");
        assert_eq!(parse("2026-09-04T24:00:00Z"), None, "hour 24");
        assert_eq!(parse("not a timestamp at all"), None);
    }

    /// A play counted in the wrong hour is a rounding error. A log entry dated
    /// to the wrong day is a user unable to answer "did it do this overnight".
    #[test]
    fn a_known_date_formats_exactly() {
        assert_eq!(format(1_788_480_000), "2026-09-04T00:00:00Z");
    }
}
