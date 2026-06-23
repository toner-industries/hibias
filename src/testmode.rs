//! Whether hibias is being driven by an automated test harness (the VHS tape,
//! end-to-end runs) rather than a real person. Off by default; turned on with
//! `HIBIAS_TEST=1`.
//!
//! Under test we make the UI deterministic and free of things a harness can't
//! control — today that means skipping album art (no image-protocol probe, no
//! network cover fetch, stable text-only frames).
//!
//! This is deliberately the ONLY thing that disables art, and it plainly means
//! "a test is driving me." There is no user-facing "turn off art" setting, so a
//! real user can never accidentally lose their album art — running the app
//! normally always shows it. New test-only behaviors should gate on
//! [`under_test`] rather than inventing their own env var.

/// True when `HIBIAS_TEST` names a truthy value.
pub fn under_test() -> bool {
    is_truthy(std::env::var("HIBIAS_TEST").ok().as_deref())
}

/// Env values that count as "on". Empty, `0`, and `false` are off, so a stray
/// or blank `HIBIAS_TEST` doesn't silently change behavior.
fn is_truthy(value: Option<&str>) -> bool {
    match value {
        Some(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_truthy;

    #[test]
    fn only_explicit_truthy_values_enable_test_mode() {
        for on in ["1", "true", "TRUE", "yes", "on"] {
            assert!(is_truthy(Some(on)), "{on:?} should enable test mode");
        }
        for off in ["", "0", "false", "False"] {
            assert!(!is_truthy(Some(off)), "{off:?} should not enable test mode");
        }
        assert!(!is_truthy(None), "unset should not enable test mode");
    }
}
