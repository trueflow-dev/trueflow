pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const LONG_VERSION: &str = VERSION;
pub const HELP_FOOTER: &str = concat!("Version: ", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_version_is_only_the_package_version() {
        assert_eq!(LONG_VERSION, VERSION);
    }

    #[test]
    fn help_footer_is_only_the_package_version() {
        assert_eq!(HELP_FOOTER, format!("Version: {VERSION}"));
    }
}
