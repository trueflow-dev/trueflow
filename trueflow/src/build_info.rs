pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit: ",
    env!("TRUEFLOW_GIT_COMMIT"),
    "\nbuilt: ",
    env!("TRUEFLOW_BUILD_TIMESTAMP")
);
pub const HELP_FOOTER: &str = concat!(
    "Version: ",
    env!("CARGO_PKG_VERSION"),
    "\nCommit: ",
    env!("TRUEFLOW_GIT_COMMIT"),
    "\nBuilt: ",
    env!("TRUEFLOW_BUILD_TIMESTAMP")
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_metadata::UNKNOWN_BUILD_TIMESTAMP;
    use chrono::DateTime;

    #[test]
    fn long_version_contains_all_metadata() {
        assert!(LONG_VERSION.contains(VERSION));
        assert!(LONG_VERSION.contains(env!("TRUEFLOW_GIT_COMMIT")));
        assert!(LONG_VERSION.contains(env!("TRUEFLOW_BUILD_TIMESTAMP")));
    }

    #[test]
    fn build_timestamp_is_unknown_or_rfc3339() {
        let build_timestamp = env!("TRUEFLOW_BUILD_TIMESTAMP");
        if build_timestamp == UNKNOWN_BUILD_TIMESTAMP {
            return;
        }

        DateTime::parse_from_rfc3339(build_timestamp)
            .unwrap_or_else(|error| panic!("build timestamp was not RFC3339: {error}"));
    }
}
