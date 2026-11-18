//! The `Date` header in IMF-fixdate form (RFC 9110, section 5.6.7), `Sun, 06 Nov 1994 08:49:37 GMT`,
//! read as Unix seconds. The node's wall clock is set from it (system design, section 1D, step 2),
//! so anything that is not exactly that form, or not a date that exists, is refused.

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The number `text` writes in exactly `text.len()` digits.
fn digits(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Days from 1970-01-01 to the given date of the Gregorian calendar; `month` is 1 to 12.
fn days_from_epoch(year: u64, month: u64, day: u64) -> u64 {
    // Count in years that begin in March, so a leap day is the last day of its year.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year % 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The Unix seconds of an IMF-fixdate, or `None` when `text` is not one. The year is 1970 or
/// later, the day exists in its month, the day's name is the date's, and a leap second (`:60`)
/// counts as the second after `:59`.
pub fn parse(text: &str) -> Option<u64> {
    // "Sun, 06 Nov 1994 08:49:37 GMT": fixed columns.
    if text.len() != 29 || !text.is_ascii() {
        return None;
    }
    let separators = [
        (3, ","),
        (4, " "),
        (7, " "),
        (11, " "),
        (16, " "),
        (19, ":"),
        (22, ":"),
        (25, " GMT"),
    ];
    if separators
        .iter()
        .any(|(at, separator)| !text[*at..].starts_with(separator))
    {
        return None;
    }
    let name = DAYS.iter().position(|day| *day == &text[0..3])? as u64;
    let day = digits(&text[5..7])?;
    let month = MONTHS.iter().position(|month| *month == &text[8..11])? as u64 + 1;
    let year = digits(&text[12..16])?;
    let hour = digits(&text[17..19])?;
    let minute = digits(&text[20..22])?;
    let second = digits(&text[23..25])?;

    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if year < 1970 || day == 0 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_epoch(year, month, day);
    // 1970-01-01 was a Thursday.
    if (days + 4) % 7 != name {
        return None;
    }
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}
