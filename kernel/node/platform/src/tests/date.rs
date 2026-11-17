//! The `Date` header's parser.

use crate::date::parse;

#[test]
fn an_imf_fixdate_is_its_unix_seconds() {
    for (text, seconds) in [
        // RFC 9110's own example.
        ("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
        ("Thu, 01 Jan 1970 00:00:00 GMT", 0),
        // The recorded answers' dates.
        ("Tue, 17 Nov 2026 09:30:00 GMT", 1_794_907_800),
        ("Tue, 17 Nov 2026 09:30:02 GMT", 1_794_907_802),
        // Leap days, in a year divisible by 4 and in one divisible by 400.
        ("Thu, 29 Feb 2024 12:00:00 GMT", 1_709_208_000),
        ("Fri, 01 Mar 2024 00:00:00 GMT", 1_709_251_200),
        ("Tue, 29 Feb 2000 00:00:00 GMT", 951_782_400),
        // The day after the 28th of February in a year divisible by 100 and not by 400.
        ("Mon, 01 Mar 2100 00:00:00 GMT", 4_107_542_400),
        // The last second of a year, and past 2^31.
        ("Thu, 31 Dec 2026 23:59:59 GMT", 1_798_761_599),
        ("Tue, 19 Jan 2038 03:14:08 GMT", 2_147_483_648),
        ("Fri, 31 Dec 9999 23:59:59 GMT", 253_402_300_799),
    ] {
        assert_eq!(parse(text), Some(seconds), "{text}");
    }
}

#[test]
fn a_leap_second_is_the_second_after() {
    assert_eq!(
        parse("Sat, 31 Dec 2016 23:59:60 GMT"),
        parse("Sun, 01 Jan 2017 00:00:00 GMT")
    );
    assert_eq!(parse("Sat, 31 Dec 2016 23:59:60 GMT"), Some(1_483_228_800));
}

#[test]
fn every_month_is_read() {
    let firsts = [
        ("Thu, 01 Jan 2026", 0),
        ("Sun, 01 Feb 2026", 31),
        ("Sun, 01 Mar 2026", 59),
        ("Wed, 01 Apr 2026", 90),
        ("Fri, 01 May 2026", 120),
        ("Mon, 01 Jun 2026", 151),
        ("Wed, 01 Jul 2026", 181),
        ("Sat, 01 Aug 2026", 212),
        ("Tue, 01 Sep 2026", 243),
        ("Thu, 01 Oct 2026", 273),
        ("Sun, 01 Nov 2026", 304),
        ("Tue, 01 Dec 2026", 334),
    ];
    // 2026-01-01 00:00:00 UTC.
    let year = 1_767_225_600;
    for (date, day) in firsts {
        let text = std::format!("{date} 00:00:00 GMT");
        assert_eq!(parse(&text), Some(year + day * 86_400), "{text}");
    }
}

#[test]
fn anything_else_is_refused() {
    for text in [
        "",
        "Tue, 17 Nov 2026 09:30:00",
        // The other forms HTTP once allowed.
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
        // Not the form's columns, cases or zone.
        "Sun, 6 Nov 1994 08:49:37 GMT",
        "Sun,  6 Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 8:49:37 GMT ",
        " Sun, 06 Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 08:49:37 GMT ",
        "Sun, 06 Nov 1994 08:49:37 UTC",
        "Sun, 06 Nov 1994 08:49:37 gmt",
        "Sun, 06 Nov 1994 08:49:37+0000",
        "Sun 06 Nov 1994 08:49:37 GMT.",
        "Sun, 06 nov 1994 08:49:37 GMT",
        "sun, 06 Nov 1994 08:49:37 GMT",
        "Sun, 06 NOV 1994 08:49:37 GMT",
        "Sun, 06-Nov-1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 08.49.37 GMT",
        "Sun, 06 Nov 1994T08:49:37 GMT",
        // Not numbers.
        "Sun, +6 Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 08:49:-7 GMT",
        "Sun, 06 Nov 19x4 08:49:37 GMT",
        "Sun, 06 Nov 1994 0\u{e9}:9:37 GMT",
        // Not a date or a time that exists.
        "Sun, 00 Nov 1994 08:49:37 GMT",
        "Thu, 31 Nov 1994 08:49:37 GMT",
        "Wed, 29 Feb 2023 00:00:00 GMT",
        "Mon, 29 Feb 2100 00:00:00 GMT",
        "Sun, 06 Nov 1994 24:00:00 GMT",
        "Sun, 06 Nov 1994 08:60:00 GMT",
        "Sun, 06 Nov 1994 08:49:61 GMT",
        "Sun, 06 Foo 1994 08:49:37 GMT",
        // The day's name is not the date's.
        "Mon, 06 Nov 1994 08:49:37 GMT",
        "Foo, 06 Nov 1994 08:49:37 GMT",
        // Before the clock's first second.
        "Wed, 31 Dec 1969 23:59:59 GMT",
        "Mon, 01 Jan 0000 00:00:00 GMT",
    ] {
        assert_eq!(parse(text), None, "{text:?}");
    }
}
