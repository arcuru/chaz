//! Small shared helpers.

/// Truncate `s` to at most `max_chars` characters. Byte-slicing `&s[..n]`
/// on arbitrary UTF-8 panics if `n` falls mid-codepoint; this is the
/// char-aware equivalent.
pub(crate) fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_codepoint_boundaries() {
        // "héllo" is 6 bytes; byte-slice [..3] would land mid-é and panic.
        assert_eq!(truncate_chars("héllo", 3), "hél");
    }

    #[test]
    fn em_dash_boundary() {
        // Regression: "— " is a 3-byte em-dash. Byte index 60 of a string
        // like "... honest — there..." falls mid-dash.
        let s = "Okay, I tried spawning a joke bot, but I have to be honest — there's a limitation";
        assert_eq!(truncate_chars(s, 60).chars().count(), 60);
    }

    #[test]
    fn noop_when_under_limit() {
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn empty_input() {
        assert_eq!(truncate_chars("", 5), "");
    }
}
