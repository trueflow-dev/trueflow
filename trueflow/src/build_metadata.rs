pub const UNKNOWN_BUILD_TIMESTAMP: &str = "unknown";
const SECONDS_PER_DAY: u64 = 86_400;
const SECONDS_PER_HOUR: u64 = 3_600;
const SECONDS_PER_MINUTE: u64 = 60;

pub fn build_timestamp_from_source_date_epoch(source_date_epoch: Option<&str>) -> String {
    let Some(source_date_epoch) = source_date_epoch else {
        return UNKNOWN_BUILD_TIMESTAMP.to_string();
    };
    let Ok(epoch_seconds) = source_date_epoch.trim().parse::<u64>() else {
        return UNKNOWN_BUILD_TIMESTAMP.to_string();
    };

    format_unix_timestamp_rfc3339(epoch_seconds)
}

fn format_unix_timestamp_rfc3339(epoch_seconds: u64) -> String {
    let days_since_unix_epoch = epoch_seconds / SECONDS_PER_DAY;
    let seconds_since_midnight = epoch_seconds % SECONDS_PER_DAY;
    let (year, month, day) = civil_from_days(days_since_unix_epoch);

    let hour = seconds_since_midnight / SECONDS_PER_HOUR;
    let minute = (seconds_since_midnight % SECONDS_PER_HOUR) / SECONDS_PER_MINUTE;
    let second = seconds_since_midnight % SECONDS_PER_MINUTE;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_unix_epoch: u64) -> (i64, i64, i64) {
    let days_since_unix_epoch = i64::try_from(days_since_unix_epoch)
        .unwrap_or_else(|_| panic!("unix timestamp day count exceeded i64"));
    let z = days_since_unix_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }

    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_source_date_epoch_zero_as_unix_epoch() {
        assert_eq!(
            build_timestamp_from_source_date_epoch(Some("0")),
            "1970-01-01T00:00:00Z"
        );
    }

    #[test]
    fn formats_known_leap_day_timestamp() {
        assert_eq!(
            build_timestamp_from_source_date_epoch(Some("1709210096")),
            "2024-02-29T12:34:56Z"
        );
    }

    #[test]
    fn falls_back_to_unknown_when_epoch_is_missing() {
        assert_eq!(
            build_timestamp_from_source_date_epoch(None),
            UNKNOWN_BUILD_TIMESTAMP
        );
    }

    #[test]
    fn falls_back_to_unknown_when_epoch_is_invalid() {
        assert_eq!(
            build_timestamp_from_source_date_epoch(Some("not-a-number")),
            UNKNOWN_BUILD_TIMESTAMP
        );
    }
}
