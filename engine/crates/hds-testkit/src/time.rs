//! NTFS timestamps.
//!
//! NTFS stores times as FILETIME: 100 ns ticks since 1601-01-01 00:00:00 UTC
//! (docs/notes/ntfs-on-disk.md section 6). The builder keeps them as plain
//! `u64` values so that edge values (0, values with the top bit set) can be
//! written as easily as real dates.

/// FILETIME ticks between 1601-01-01 and 1970-01-01.
pub const UNIX_EPOCH: u64 = 116_444_736_000_000_000;
/// Ticks per second.
pub const SECOND: u64 = 10_000_000;
/// Ticks per minute.
pub const MINUTE: u64 = 60 * SECOND;
/// Ticks per hour.
pub const HOUR: u64 = 60 * MINUTE;
/// Ticks per day.
pub const DAY: u64 = 24 * HOUR;

/// FILETIME of a civil UTC date and time (proleptic Gregorian calendar).
///
/// Panics for dates before 1601-01-01 or with fields out of range: the
/// arguments are literals in test code.
pub fn ymd_hms(year: u32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> u64 {
    assert!(year >= 1601, "FILETIME starts in 1601");
    assert!(
        (1..=12).contains(&month) && (1..=31).contains(&day),
        "bad date"
    );
    assert!(hour < 24 && minute < 60 && second < 60, "bad time of day");
    days_since_1601(year, month, day) * DAY
        + hour as u64 * HOUR
        + minute as u64 * MINUTE
        + second as u64 * SECOND
}

/// FILETIME of midnight UTC on a civil date.
pub fn ymd(year: u32, month: u32, day: u32) -> u64 {
    ymd_hms(year, month, day, 0, 0, 0)
}

fn days_since_1601(year: u32, month: u32, day: u32) -> u64 {
    // Days from civil, after Howard Hinnant's well-known algorithm, shifted
    // from the 0000-03-01 era start to 1601-01-01.
    let y = if month <= 2 {
        year as i64 - 1
    } else {
        year as i64
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_0000_03_01 = era * 146_097 + doe;
    // 1601-01-01 is 584_694 days after 0000-03-01.
    (days_from_0000_03_01 - 584_694) as u64
}

/// FILETIME of a Unix time in milliseconds (must not be before 1601).
pub fn from_unix_ms(ms: i64) -> u64 {
    let ticks = UNIX_EPOCH as i128 + ms as i128 * 10_000;
    u64::try_from(ticks).expect("time before 1601")
}

/// Unix milliseconds of a FILETIME, rounded towards minus infinity, the way
/// PROTOCOL.md reports times. `None` for values that are not real times
/// (0 or the top bit set; ALGORITHMS H8).
pub fn to_unix_ms(filetime: u64) -> Option<i64> {
    if filetime == 0 || filetime >= 1 << 63 {
        return None;
    }
    Some((filetime as i64 - UNIX_EPOCH as i64).div_euclid(10_000))
}

/// `YYYY-MM-DDTHH:MM:SS.fffffffZ` for diagnostics; raw number for values
/// that are not representable.
pub fn to_iso(filetime: u64) -> String {
    if filetime >= 1 << 63 {
        return format!("filetime:{filetime:#x}");
    }
    let days = filetime / DAY;
    let rest = filetime % DAY;
    // Civil from days (Hinnant), shifted to the 1601 epoch.
    let z = days as i64 + 584_694;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}Z",
        year,
        month,
        day,
        rest / HOUR,
        rest % HOUR / MINUTE,
        rest % MINUTE / SECOND,
        rest % SECOND
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants() {
        assert_eq!(ymd(1601, 1, 1), 0);
        assert_eq!(ymd(1970, 1, 1), UNIX_EPOCH);
        // Raw creation time of `passwords.txt` on the Windows 10 sample volume
        // of docs/notes (bytes 86 A3 F1 EF C2 9F D6 01), which an independent
        // reader prints as 2020-10-11T11:37:45.2070790Z.
        assert_eq!(
            ymd_hms(2020, 10, 11, 11, 37, 45) + 2_070_790,
            0x01D6_9FC2_EFF1_A386
        );
        // computed with Python's datetime
        assert_eq!(
            ymd_hms(2020, 10, 11, 11, 37, 44) + 3_570_445,
            132_468_898_643_570_445
        );
        assert_eq!(ymd_hms(2024, 2, 29, 23, 59, 59), 133_537_247_990_000_000);
        // 2000-02-29 exists, 1900-02-29 does not: day arithmetic across both.
        assert_eq!(ymd(2000, 3, 1) - ymd(2000, 2, 28), 2 * DAY);
        assert_eq!(ymd(1900, 3, 1) - ymd(1900, 2, 28), DAY);
        assert_eq!(from_unix_ms(0), UNIX_EPOCH);
        assert_eq!(from_unix_ms(1_602_416_264_357), 132_468_898_643_570_000);
        assert_eq!(from_unix_ms(-1), UNIX_EPOCH - 10_000);
    }

    #[test]
    fn unix_ms_round_trip_and_edges() {
        assert_eq!(to_unix_ms(UNIX_EPOCH), Some(0));
        assert_eq!(to_unix_ms(UNIX_EPOCH - 1), Some(-1));
        assert_eq!(to_unix_ms(UNIX_EPOCH + 9_999), Some(0));
        assert_eq!(to_unix_ms(0), None);
        assert_eq!(to_unix_ms(1 << 63), None);
        assert_eq!(to_unix_ms(ymd(1601, 1, 2)), Some(-11_644_387_200_000));
    }

    #[test]
    fn iso_formatting() {
        assert_eq!(to_iso(0), "1601-01-01T00:00:00.0000000Z");
        assert_eq!(
            to_iso(132_468_898_643_570_445),
            "2020-10-11T11:37:44.3570445Z"
        );
        assert_eq!(
            to_iso(0x01D6_9FC2_EFF1_A386),
            "2020-10-11T11:37:45.2070790Z"
        );
        assert_eq!(
            to_iso(ymd_hms(2024, 2, 29, 23, 59, 59)),
            "2024-02-29T23:59:59.0000000Z"
        );
        assert_eq!(to_iso(ymd(1999, 12, 31)), "1999-12-31T00:00:00.0000000Z");
        assert_eq!(to_iso(u64::MAX), "filetime:0xffffffffffffffff");
        for (y, m, d) in [
            (1601, 1, 1),
            (1700, 3, 1),
            (1970, 1, 1),
            (2038, 1, 19),
            (2100, 12, 31),
        ] {
            let iso = to_iso(ymd(y, m, d));
            assert_eq!(iso, format!("{y:04}-{m:02}-{d:02}T00:00:00.0000000Z"));
        }
    }
}
