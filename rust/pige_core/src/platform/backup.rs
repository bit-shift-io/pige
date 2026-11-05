//! Naming for the configuration backups a person can see in their Downloads
//! folder. Adapted from qlaunch's `platform/backup.rs` (bitshift, MIT).
//!
//! A backup carries the UTC date and time in its name, so backups sort by name
//! and two written in the same minute cannot overwrite each other.

/// The part every backup file name starts with.
pub const BACKUP_PREFIX: &str = "pige-backup-";

/// The part every backup file name ends with.
pub const BACKUP_SUFFIX: &str = ".json";

/// `pige-backup-YYYY-MM-DD-HHMMSS.json` for `now_ms` milliseconds after the
/// Unix epoch, in UTC.
pub fn backup_name(now_ms: i64) -> String {
    let seconds = now_ms.div_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let seconds_today = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{BACKUP_PREFIX}{year:04}-{month:02}-{day:02}-{:02}{:02}{:02}{BACKUP_SUFFIX}",
        seconds_today / 3_600,
        (seconds_today % 3_600) / 60,
        seconds_today % 60,
    )
}

/// The calendar date for `days` since the Unix epoch, using Howard Hinnant's
/// `civil_from_days` (public domain). Negative days (before 1970) work.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Whether `name` is a backup this program wrote and may read back. The check
/// is deliberately strict: the name must be exactly the prefix, a UTC stamp
/// with no separators of its own, and the suffix. That keeps a name from the
/// Downloads listing from naming a file anywhere but there.
pub fn is_backup_name(name: &str) -> bool {
    let Some(stamp) = name
        .strip_prefix(BACKUP_PREFIX)
        .and_then(|rest| rest.strip_suffix(BACKUP_SUFFIX))
    else {
        return false;
    };
    let bytes = stamp.as_bytes();
    // YYYY-MM-DD-HHMMSS is 17 bytes with three dashes and nothing else.
    bytes.len() == 17
        && bytes.iter().enumerate().all(|(i, b)| {
            if matches!(i, 4 | 7 | 10) {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// The backup names among `names`, newest first. Everything else is dropped, so
/// a Downloads listing cannot smuggle in another file.
pub fn sort_backup_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = names
        .into_iter()
        .filter(|name| is_backup_name(name))
        .collect();
    // The stamp is fixed-width, so newest-first is a plain reverse sort.
    names.sort_unstable_by(|a, b| b.cmp(a));
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_the_prefix_a_utc_stamp_and_the_suffix() {
        // 2026-10-21 14:05:30 UTC.
        let ms = 1_792_591_530_000;
        assert_eq!(backup_name(ms), "pige-backup-2026-10-21-140530.json");
    }

    #[test]
    fn names_from_the_epoch_and_before_it_still_have_a_date() {
        assert_eq!(backup_name(0), "pige-backup-1970-01-01-000000.json");
        assert_eq!(backup_name(-1_000), "pige-backup-1969-12-31-235959.json");
    }

    #[test]
    fn a_listing_keeps_only_backups_newest_first() {
        let names = [
            "holiday.jpg".to_string(),
            "pige-backup-2026-10-20-090000.json".to_string(),
            "pige-backup-2026-10-21-140530.json".to_string(),
        ];
        assert_eq!(
            sort_backup_names(names),
            [
                "pige-backup-2026-10-21-140530.json",
                "pige-backup-2026-10-20-090000.json",
            ]
        );
    }

    #[test]
    fn only_the_exact_shape_is_a_backup_name() {
        assert!(is_backup_name("pige-backup-2026-10-21-140530.json"));
        for bad in [
            "",
            "pige-backup-.json",
            "pige-backup-2026-10-21-1405.json",
            "pige-backup-2026-10-21-140530.JSON",
            "pige-backup-2026-10-21-140530.json.bak",
            "../pige-backup-2026-10-21-140530.json",
            "pige-backup-2026-10-21-140530/evil.json",
            "pige-backup-2026-10-21-14053a.json",
        ] {
            assert!(!is_backup_name(bad), "{bad:?} should be rejected");
        }
    }
}
