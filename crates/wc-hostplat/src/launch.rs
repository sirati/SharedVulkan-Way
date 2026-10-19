//! How the host starts an application for Linux: the argv, built here so it
//! is tested where the crate is built. Never a command string and never a
//! shell: each element is one argument, which `std::process::Command`
//! quotes by the platform's rules.

/// The arguments for the configured browser: its fixed arguments, then the
/// URL as one element of its own. The URL was validated and re-serialised
/// by `wc_hostint::openurl::validate_url` (an `http(s)` scheme first, so it
/// can never be read as an option).
pub fn browser_argv(fixed: &[String], url: &str) -> Vec<String> {
    let mut a: Vec<String> = fixed.to_vec();
    a.push(url.to_string());
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_is_one_element_after_the_fixed_arguments() {
        let fixed: Vec<String> =
            ["--user-data-dir=C:\\Temp\\wc edge", "--no-first-run", "--no-default-browser-check", "--new-window"].map(String::from).to_vec();
        let a = browser_argv(&fixed, "https://example.org/a b?q=\"x\"&y");
        assert_eq!(a.len(), 5);
        assert_eq!(a[..4], fixed[..]);
        assert_eq!(a[4], "https://example.org/a b?q=\"x\"&y", "untouched: Command quotes it");
        assert_eq!(browser_argv(&[], "https://a/"), ["https://a/"]);
    }
}
