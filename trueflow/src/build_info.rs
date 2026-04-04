pub const VERSION: &str = env!("CARGO_PKG_VERSION");
#[allow(dead_code)]
pub const COMMIT_HASH: &str = env!("TRUEFLOW_GIT_COMMIT");
#[allow(dead_code)]
pub const BUILD_TIMESTAMP: &str = env!("TRUEFLOW_BUILD_TIMESTAMP");
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
    use chrono::DateTime;

    #[test]
    fn long_version_contains_all_metadata() {
        assert!(LONG_VERSION.contains(VERSION));
        assert!(LONG_VERSION.contains(COMMIT_HASH));
        assert!(LONG_VERSION.contains(BUILD_TIMESTAMP));
    }

    #[test]
    fn build_timestamp_is_rfc3339() {
        DateTime::parse_from_rfc3339(BUILD_TIMESTAMP)
            .unwrap_or_else(|error| panic!("build timestamp was not RFC3339: {error}"));
    }
}
