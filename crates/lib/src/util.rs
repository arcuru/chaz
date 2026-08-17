//! Small shared helpers.

use std::path::{Path, PathBuf};

/// Expand a leading `~` / `~/…` in `path` against the home directory.
/// Returns `path` unchanged when there is no leading tilde or no home dir.
///
/// Config paths arrive as whatever the operator typed, and a shell only
/// expands a tilde it sees itself — one inside a YAML string reaches us
/// literally and turns into a directory named `~`.
pub fn expand_home(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Truncate `s` to at most `max_chars` characters. Byte-slicing `&s[..n]`
/// on arbitrary UTF-8 panics if `n` falls mid-codepoint; this is the
/// char-aware equivalent.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_rewrites_leading_tilde() {
        let home = dirs::home_dir().expect("home dir in test env");
        assert_eq!(
            expand_home(Path::new("~/brain/x.md")),
            home.join("brain/x.md")
        );
        assert_eq!(expand_home(Path::new("~")), home);
        // No tilde → untouched; mid-path tilde is not expanded.
        assert_eq!(
            expand_home(Path::new("/abs/p.md")),
            PathBuf::from("/abs/p.md")
        );
        assert_eq!(expand_home(Path::new("/a/~/b")), PathBuf::from("/a/~/b"));
    }

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
