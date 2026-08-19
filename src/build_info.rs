//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }

    /// FORK BRANCH ASSERTION. The deployed binary must not present itself as
    /// stock, and a test that accepts either identity cannot detect it doing
    /// so — which is how a CI or `cargo install` build silently ships stock.
    /// `build.rs` stamps the channel unconditionally and is tracked, so this
    /// is exact rather than conditional.
    ///
    /// On an upstream tree without that stamp this test fails, which is
    /// correct: it is a fork-identity assertion and does not belong upstream.
    #[test]
    fn the_fork_identity_is_exact() {
        assert_eq!(super::channel(), "heb");
        assert_eq!(super::build_id(), Some("1"));
        assert_eq!(
            super::version(),
            concat!(env!("CARGO_PKG_VERSION"), "-heb.1"),
            "the fork must report its own identity, never stock"
        );
        assert_ne!(super::version(), super::BASE_VERSION);
    }
}
