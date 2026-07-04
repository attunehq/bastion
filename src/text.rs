//! Small, surface-neutral string helpers shared across the output paths.
//!
//! These format text the same way regardless of transport, so both the local
//! renderer ([`crate::render`]) and the GitHub adapter ([`crate::github`]) can
//! depend on them without either reaching into the other's module.

/// Truncate `text` to at most `max` characters, appending an ASCII ellipsis (`...`)
/// when a cut is made (the ellipsis is kept within `max`). Shared by the local
/// finding renderer and the GitHub check-run titles so both surfaces cap text the
/// same way. Callers that need leading/trailing whitespace ignored trim first.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(3)).collect();
    format!("{}...", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_returned_unchanged() {
        assert_eq!(truncate("short", 110), "short");
        // Exactly at the limit is not a cut.
        assert_eq!(truncate("abcde", 5), "abcde");
    }

    #[test]
    fn overlong_text_is_cut_with_an_ellipsis_kept_within_max() {
        let long = "a".repeat(200);
        let cut = truncate(&long, 110);
        assert!(cut.ends_with("..."));
        assert_eq!(cut.chars().count(), 110);
    }

    #[test]
    fn counts_characters_not_bytes() {
        // A multi-byte char counts as one, so the cap is on characters, not bytes.
        let wide = "é".repeat(10);
        assert_eq!(truncate(&wide, 20), wide);
    }
}
