//! Temporal parts of `moruna.expr` (spec/expr.md, "Temporal parts"): the civil calendar of a
//! timestamp's local time, proleptic Gregorian, with floor division so an instant before 1970
//! falls in the day, hour and second it lies in.

use std::str::FromStr;

use moruna_kernel::arrow::array::temporal_conversions::as_datetime_with_timezone;
use moruna_kernel::arrow::array::timezone::Tz;
use moruna_kernel::arrow::datatypes::{
    TimeUnit, TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType,
};

/// A part of a date or a time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// The year.
    Year,
    /// The quarter, 1 to 4.
    Quarter,
    /// The month, 1 to 12.
    Month,
    /// The day of the month, 1 to 31.
    Day,
    /// The day of the week: `week_start` (1 Monday to 7 Sunday) is 0, or 1 when not counting from
    /// zero.
    DayOfWeek {
        /// The first day counts 0, not 1.
        count_from_zero: bool,
        /// The first day of the week, 1 (Monday) to 7 (Sunday).
        week_start: u8,
    },
    /// The day of the year, 1 to 366.
    DayOfYear,
    /// The ISO 8601 week-numbering year.
    IsoYear,
    /// The ISO 8601 week, 1 to 53.
    IsoWeek,
    /// The hour, 0 to 23.
    Hour,
    /// The minute, 0 to 59.
    Minute,
    /// The second, 0 to 59.
    Second,
    /// The millisecond within the second, 0 to 999.
    Millisecond,
    /// The microsecond within the millisecond, 0 to 999.
    Microsecond,
    /// The nanosecond within the microsecond, 0 to 999.
    Nanosecond,
}

impl Part {
    /// The part Arrow's function `name` extracts, with its default options.
    pub fn named(name: &str) -> Option<Part> {
        Some(match name {
            "year" => Part::Year,
            "quarter" => Part::Quarter,
            "month" => Part::Month,
            "day" => Part::Day,
            "day_of_week" => Part::DayOfWeek {
                count_from_zero: true,
                week_start: 1,
            },
            "day_of_year" => Part::DayOfYear,
            "iso_year" => Part::IsoYear,
            "iso_week" => Part::IsoWeek,
            "hour" => Part::Hour,
            "minute" => Part::Minute,
            "second" => Part::Second,
            "millisecond" => Part::Millisecond,
            "microsecond" => Part::Microsecond,
            "nanosecond" => Part::Nanosecond,
            _ => return None,
        })
    }

    /// True for a part of the time of day, which a date does not have.
    pub fn of_time(self) -> bool {
        matches!(
            self,
            Part::Hour
                | Part::Minute
                | Part::Second
                | Part::Millisecond
                | Part::Microsecond
                | Part::Nanosecond
        )
    }
}

/// Every temporal function's name.
pub const NAMES: [&str; 14] = [
    "year",
    "quarter",
    "month",
    "day",
    "day_of_week",
    "day_of_year",
    "iso_year",
    "iso_week",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
];

/// The count of a unit in one second.
pub fn per_second(unit: TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date of a count of days since 1970-01-01 (`civil_from_days`).
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A part of a date, from its days since 1970-01-01. `None` for a part of the time of day.
pub fn of_days(days: i64, part: Part) -> Option<i64> {
    // Garbage under a null may be any value; keep the arithmetic in range.
    let days = days.clamp(-(1 << 40), 1 << 40);
    let (y, m, d) = civil_from_days(days);
    Some(match part {
        Part::Year => y,
        Part::Quarter => (m - 1) / 3 + 1,
        Part::Month => m,
        Part::Day => d,
        Part::DayOfWeek {
            count_from_zero,
            week_start,
        } => {
            let monday0 = (days + 3).rem_euclid(7);
            (monday0 - (i64::from(week_start) - 1)).rem_euclid(7) + i64::from(!count_from_zero)
        }
        Part::DayOfYear => days - days_from_civil(y, 1, 1) + 1,
        Part::IsoYear => civil_from_days(iso_thursday(days)).0,
        Part::IsoWeek => {
            let thursday = iso_thursday(days);
            let year = civil_from_days(thursday).0;
            (thursday - days_from_civil(year, 1, 1)) / 7 + 1
        }
        _ => return None,
    })
}

/// The Thursday of the ISO week a day is in, which names the week's year.
fn iso_thursday(days: i64) -> i64 {
    days - (days + 3).rem_euclid(7) + 3
}

/// A part of a local time, `local` counted in `unit` from 1970-01-01T00:00 local.
pub fn of_local(local: i64, unit: TimeUnit, part: Part) -> i64 {
    let f = per_second(unit);
    let secs = local.div_euclid(f);
    let sub = local.rem_euclid(f);
    let nanos = sub * (1_000_000_000 / f);
    let sod = secs.rem_euclid(86_400);
    match part {
        Part::Hour => sod / 3600,
        Part::Minute => sod % 3600 / 60,
        Part::Second => sod % 60,
        Part::Millisecond => nanos / 1_000_000,
        Part::Microsecond => nanos / 1_000 % 1_000,
        Part::Nanosecond => nanos % 1_000,
        date => of_days(secs.div_euclid(86_400), date).unwrap_or(0),
    }
}

/// A timestamp's zone, as the parts see it.
#[derive(Clone, Debug)]
pub enum Zone {
    /// No zone, or UTC: the stored value is the local time.
    Utc,
    /// A zone whose offset may vary: looked up for every value.
    Named(Tz),
}

impl Zone {
    /// The zone a timestamp type names; an unknown zone is an error naming it.
    pub fn of(tz: Option<&str>) -> Result<Zone, String> {
        match tz {
            None => Ok(Zone::Utc),
            Some("UTC" | "Etc/UTC" | "Z" | "+00:00" | "-00:00" | "+0000" | "+00") => Ok(Zone::Utc),
            Some(name) => Tz::from_str(name)
                .map(Zone::Named)
                .map_err(|e| format!("the zone `{name}` is not known: {e}")),
        }
    }

    /// The local time of instant `v` (in `unit`), in `unit`; `None` past chrono's range.
    pub fn local(&self, v: i64, unit: TimeUnit) -> Option<i64> {
        let Zone::Named(tz) = self else {
            return Some(v);
        };
        let tz = *tz;
        let dt = match unit {
            TimeUnit::Second => as_datetime_with_timezone::<TimestampSecondType>(v, tz),
            TimeUnit::Millisecond => as_datetime_with_timezone::<TimestampMillisecondType>(v, tz),
            TimeUnit::Microsecond => as_datetime_with_timezone::<TimestampMicrosecondType>(v, tz),
            TimeUnit::Nanosecond => as_datetime_with_timezone::<TimestampNanosecondType>(v, tz),
        }?;
        let offset = dt.naive_local().and_utc().timestamp() - dt.timestamp();
        offset.checked_mul(per_second(unit))?.checked_add(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: TimeUnit = TimeUnit::Microsecond;

    fn all(v: i64, unit: TimeUnit) -> Vec<i64> {
        NAMES
            .iter()
            .map(|n| of_local(v, unit, Part::named(n).expect(n)))
            .collect()
    }

    #[test]
    fn parts_match_arrow_on_both_sides_of_the_epoch() {
        // pyarrow 24 on timestamp[us]: -1, 0, and 2024-01-01T17:00:00.123456.
        assert_eq!(
            all(-1, US),
            vec![1969, 4, 12, 31, 2, 365, 1970, 1, 23, 59, 59, 999, 999, 0]
        );
        assert_eq!(
            all(0, US),
            vec![1970, 1, 1, 1, 3, 1, 1970, 1, 0, 0, 0, 0, 0, 0]
        );
        let t = 1_704_067_200_000_000 + 17 * 3_600_000_000 + 123_456;
        assert_eq!(
            all(t, US),
            vec![2024, 1, 1, 1, 0, 1, 2024, 1, 17, 0, 0, 123, 456, 0]
        );
        assert_eq!(of_local(-1, TimeUnit::Second, Part::Hour), 23);
        assert_eq!(of_local(-1, TimeUnit::Nanosecond, Part::Nanosecond), 999);
        assert_eq!(
            of_local(1_234_567_891, TimeUnit::Nanosecond, Part::Nanosecond),
            891
        );
        assert_eq!(
            of_local(1_234_567_891, TimeUnit::Millisecond, Part::Millisecond),
            891
        );
        let sunday1 = Part::DayOfWeek {
            count_from_zero: false,
            week_start: 7,
        };
        // pyarrow: 1969-12-31 (a Wednesday) is 4 counting Sunday as 1.
        assert_eq!(of_local(-1, US, sunday1), 4);
        assert_eq!(of_local(0, US, sunday1), 5);
    }

    #[test]
    fn the_calendar_round_trips_and_iso_weeks_cross_years() {
        for days in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        // 2021-01-03 is in ISO week 53 of 2020; 2024-12-30 in week 1 of 2025.
        let d = days_from_civil(2021, 1, 3);
        assert_eq!(of_days(d, Part::IsoYear), Some(2020));
        assert_eq!(of_days(d, Part::IsoWeek), Some(53));
        let d = days_from_civil(2024, 12, 30);
        assert_eq!(of_days(d, Part::IsoYear), Some(2025));
        assert_eq!(of_days(d, Part::IsoWeek), Some(1));
        assert_eq!(
            of_days(days_from_civil(2024, 12, 31), Part::DayOfYear),
            Some(366)
        );
        assert_eq!(of_days(0, Part::Hour), None);
        assert_eq!(of_days(i64::MAX, Part::Year).map(|y| y > 0), Some(true));
        assert!(Part::Hour.of_time() && !Part::Day.of_time());
        assert_eq!(Part::named("week"), None);
    }

    #[test]
    fn zones_shift_to_local_time() {
        let ny = Zone::of(Some("America/New_York")).expect("zone");
        // pyarrow: hour of the epoch in New York is 19.
        let local = ny.local(0, US).expect("local");
        assert_eq!(of_local(local, US, Part::Hour), 19);
        let ist = Zone::of(Some("+05:30")).expect("zone");
        assert_eq!(of_local(ist.local(0, US).expect("l"), US, Part::Hour), 5);
        assert_eq!(of_local(ist.local(0, US).expect("l"), US, Part::Minute), 30);
        for unit in [
            TimeUnit::Second,
            TimeUnit::Millisecond,
            TimeUnit::Nanosecond,
        ] {
            assert_eq!(ny.local(0, unit), Some(-5 * 3600 * per_second(unit)));
        }
        assert_eq!(Zone::of(Some("UTC")).expect("utc").local(5, US), Some(5));
        assert!(Zone::of(Some("Mars/Olympus")).is_err());
        assert_eq!(ny.local(i64::MAX, TimeUnit::Second), None);
    }
}
