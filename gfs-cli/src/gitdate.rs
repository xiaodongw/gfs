//! Git's date formats, from a Unix timestamp and a timezone offset.
//!
//! # Why this is written out rather than taken from a crate
//!
//! The only thing needed is the civil date for a Unix second, which is Howard
//! Hinnant's `civil_from_days` — about fifteen lines and exact for the whole
//! range Git can store. A date-and-time crate would bring a timezone database,
//! a parser, and a serialization format, none of which are used, and every entry
//! in ADR 0001's dependency table has to be audited, licensed, and pinned. The
//! same reasoning produced the hand-written Myers diff in `gfs-overlay` and the
//! hand-written HTTP GET in the CLI.
//!
//! # The offset is the commit's, not the reader's
//!
//! Git stores the author's UTC offset alongside the timestamp and prints the
//! time *as the author saw it*. Rendering in the reader's local zone would make
//! two people reviewing the same commit see different times, so the stored
//! offset is applied here and printed alongside.

/// `Thu Apr 7 15:13:13 2005 -0700` — what `%ad` prints with no `--date` given.
pub fn default_format(secs: i64, tz_offset_minutes: i32) -> String {
  let (y, m, d, hh, mm, ss, wd) = civil(secs, tz_offset_minutes);
  format!(
    "{} {} {} {:02}:{:02}:{:02} {} {}",
    WEEKDAYS[wd as usize],
    MONTHS[(m - 1) as usize],
    d,
    hh,
    mm,
    ss,
    y,
    offset(tz_offset_minutes, false)
  )
}

/// `2005-04-07 15:13:13 -0700` — `%ai`.
pub fn iso(secs: i64, tz_offset_minutes: i32) -> String {
  let (y, m, d, hh, mm, ss, _) = civil(secs, tz_offset_minutes);
  format!(
    "{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} {}",
    offset(tz_offset_minutes, false)
  )
}

/// `2005-04-07T15:13:13-07:00` — `%aI`, strict ISO 8601.
pub fn iso_strict(secs: i64, tz_offset_minutes: i32) -> String {
  let (y, m, d, hh, mm, ss, _) = civil(secs, tz_offset_minutes);
  format!(
    "{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{}",
    offset(tz_offset_minutes, true)
  )
}

/// `3 weeks ago` — `%ar`.
///
/// Git's own thresholds, including the one that surprises people: past a year it
/// prints years *and* months, because "2 years ago" for something 2 years and 11
/// months old is misleading in the direction that matters.
pub fn relative(secs: i64, now: i64) -> String {
  let delta = now - secs;
  if delta < 0 {
    return "in the future".to_owned();
  }
  const MINUTE: i64 = 60;
  const HOUR: i64 = 60 * MINUTE;
  const DAY: i64 = 24 * HOUR;
  const WEEK: i64 = 7 * DAY;
  // Git's approximations, not calendar arithmetic: a "month" is a twelfth of a
  // Julian year, which is what makes `%ar` cheap and stable.
  const MONTH: i64 = 30 * DAY + 10 * HOUR + 30 * MINUTE;
  const YEAR: i64 = 365 * DAY + 6 * HOUR;

  match delta {
    d if d < MINUTE => plural(d, "second"),
    d if d < HOUR => plural(d / MINUTE, "minute"),
    d if d < DAY => plural(d / HOUR, "hour"),
    d if d < WEEK => plural(d / DAY, "day"),
    d if d < 10 * WEEK => plural(d / WEEK, "week"),
    d if d < YEAR => plural(d / MONTH, "month"),
    d => {
      let years = d / YEAR;
      let months = (d % YEAR) / MONTH;
      if months == 0 {
        plural(years, "year")
      } else {
        format!("{}, {} ago", count(years, "year"), count(months, "month"))
      }
    }
  }
}

fn plural(n: i64, unit: &str) -> String {
  format!("{} ago", count(n, unit))
}

fn count(n: i64, unit: &str) -> String {
  if n == 1 {
    format!("1 {unit}")
  } else {
    format!("{n} {unit}s")
  }
}

/// `+0200`, or `+02:00` when `colon`.
fn offset(minutes: i32, colon: bool) -> String {
  let sign = if minutes < 0 { '-' } else { '+' };
  let abs = minutes.unsigned_abs();
  let (h, m) = (abs / 60, abs % 60);
  if colon {
    format!("{sign}{h:02}:{m:02}")
  } else {
    format!("{sign}{h:02}{m:02}")
  }
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
  "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Split a Unix second, shifted by an offset, into its civil parts.
///
/// Returns `(year, month, day, hour, minute, second, weekday)`, where the
/// weekday indexes [`WEEKDAYS`] — which starts at Thursday because day 0 of the
/// Unix epoch was one.
fn civil(secs: i64, tz_offset_minutes: i32) -> (i64, i64, i64, i64, i64, i64, i64) {
  let local = secs + i64::from(tz_offset_minutes) * 60;
  // Euclidean division, so a pre-epoch timestamp floors rather than truncating
  // toward zero and lands on the previous day instead of the next one.
  let days = local.div_euclid(86_400);
  let rem = local.rem_euclid(86_400);
  let (y, m, d) = civil_from_days(days);
  (
    y,
    m,
    d,
    rem / 3600,
    (rem % 3600) / 60,
    rem % 60,
    days.rem_euclid(7),
  )
}

/// Howard Hinnant's `civil_from_days`, exact for the proleptic Gregorian
/// calendar over the whole `i64` day range.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
  // Shift the epoch to 0000-03-01, which puts the leap day at the end of the
  // year and makes the month arithmetic below branchless.
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z.rem_euclid(146_097);
  let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
  let y = yoe + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = doy - (153 * mp + 2) / 5 + 1;
  let m = if mp < 10 { mp + 3 } else { mp - 9 };
  (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_timestamps_match_git() {
    // Git's own documentation example: the first commit in git.git.
    assert_eq!(
      default_format(1_112_911_993, -420),
      "Thu Apr 7 15:13:13 2005 -0700"
    );
    assert_eq!(iso(1_112_911_993, -420), "2005-04-07 15:13:13 -0700");
    assert_eq!(iso_strict(1_112_911_993, -420), "2005-04-07T15:13:13-07:00");
    // The epoch itself, in UTC, and the day of the week it fell on.
    assert_eq!(default_format(0, 0), "Thu Jan 1 00:00:00 1970 +0000");
    // A leap day, which the branchless month arithmetic has to get right.
    assert_eq!(iso(1_709_164_800, 0), "2024-02-29 00:00:00 +0000");
    // A half-hour zone, because a zone is not a whole number of hours.
    assert_eq!(iso(0, 330), "1970-01-01 05:30:00 +0530");
  }

  #[test]
  fn pre_epoch_times_floor_rather_than_truncate() {
    // Git stores a signed timestamp and repositories with dates before 1970
    // exist. Truncating toward zero would put this on 1970-01-01.
    assert_eq!(iso(-1, 0), "1969-12-31 23:59:59 +0000");
  }

  #[test]
  fn relative_uses_gits_thresholds() {
    let now = 1_700_000_000;
    assert_eq!(relative(now, now), "0 seconds ago");
    assert_eq!(relative(now - 1, now), "1 second ago");
    assert_eq!(relative(now - 90, now), "1 minute ago");
    assert_eq!(relative(now - 3 * 3600, now), "3 hours ago");
    assert_eq!(relative(now - 3 * 86400, now), "3 days ago");
    assert_eq!(relative(now - 21 * 86400, now), "3 weeks ago");
    assert_eq!(relative(now - 200 * 86400, now), "6 months ago");
    // The years-and-months case: "2 years ago" for something nearly three years
    // old is wrong in the direction that matters.
    assert_eq!(relative(now - 1000 * 86400, now), "2 years, 8 months ago");
  }
}
