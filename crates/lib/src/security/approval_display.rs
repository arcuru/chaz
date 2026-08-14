//! Rendering primitives for approval prompts that carry peer-supplied text.
//!
//! A bootstrap request arrives from another peer and most of what it says
//! about itself is a claim: the login `kind`, the `identifier`, the bridge DB
//! it points at, even its own timestamp. Printing those verbatim lets the
//! requester choose what the approver reads — escape sequences can repaint the
//! line, a newline can forge a second request, and a bidi override can make an
//! identifier render as something other than the string that will be stored.
//!
//! Two things make the prompt defensible:
//!
//! - [`quarantine`] renders a claimed string safely: NFC-normalized, control
//!   and bidi-formatting characters neutralized, length capped.
//! - [`pubkey_fingerprint`] gives the one field the requester cannot choose —
//!   a digest of the key that signed the request — so the approver has an
//!   anchor to compare against a key they already trust.
//!
//! Quarantining is a *display* transform. The stored value is still the raw
//! string, so a rendered value is never safe to compare or key on: match on
//! the raw string and render the quarantined one.
//!
//! Scope: control characters, bidi formatting, normalization form and length.
//! Confusables and script-mixing within otherwise-printable text are not
//! addressed here; that needs an identifier profile applied at registration,
//! not at render.

use base64::Engine;
use eidetica::auth::crypto::PublicKey;
use sha2::{Digest, Sha256};
use std::fmt;
use unicode_normalization::UnicodeNormalization;

/// Characters a claimed value may occupy on screen before it is truncated.
/// Long enough for a full MXID, short enough that a claim cannot push the
/// system-derived fields out of view.
pub const DISPLAY_CAP: usize = 48;

/// Shown in place of a claimed value that is empty, or that consisted
/// entirely of characters stripped during quarantine.
const EMPTY_MARKER: &str = "(empty)";

/// A peer-supplied string prepared for display, plus what had to be done to
/// it. The flags let a caller tell the approver that what they are reading is
/// not byte-for-byte what the requester sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantined {
    /// The text safe to print.
    pub text: String,
    /// Some character was escaped, removed or replaced.
    pub altered: bool,
    /// The value was longer than the cap and lost its tail.
    pub truncated: bool,
}

impl Quarantined {
    /// A short parenthetical describing the transform, or `""` when the
    /// rendering is faithful.
    pub fn note(&self) -> &'static str {
        match (self.altered, self.truncated) {
            (true, true) => "  (escaped for display, truncated)",
            (true, false) => "  (escaped for display)",
            (false, true) => "  (truncated)",
            (false, false) => "",
        }
    }
}

impl fmt::Display for Quarantined {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Prepare a peer-supplied string for display, capped at [`DISPLAY_CAP`].
pub fn quarantine(raw: &str) -> Quarantined {
    quarantine_capped(raw, DISPLAY_CAP)
}

/// Prepare a peer-supplied string for display with an explicit cap, in
/// characters. Used for fields that are legitimately longer than a claimed
/// identifier, such as an encoded key.
///
/// The transform, in order:
///
/// 1. NFC normalization, so two spellings of the same text render the same way
///    and a decomposed sequence cannot smuggle combining marks past the cap.
/// 2. `\n` and `\r` become the visible two-character escapes `\n` and `\r`, so
///    a claim cannot forge additional lines in the prompt.
/// 3. `ESC` and the rest of the C1 range become U+FFFD — those are the
///    introducers for OSC and CSI, so neutralizing them defeats terminal
///    escape sequences without deleting evidence that something was there.
/// 4. Remaining C0 controls and `DEL` are dropped.
/// 5. Bidi overrides and isolates (U+202A–U+202E, U+2066–U+2069) and the
///    directional marks (U+200E, U+200F, U+061C) become U+FFFD, so text cannot
///    be made to render in an order other than the one it is stored in.
/// 6. The result is capped, with `…` marking the truncation.
pub fn quarantine_capped(raw: &str, cap: usize) -> Quarantined {
    let mut altered = false;
    let mut out = String::with_capacity(raw.len());

    for c in raw.nfc() {
        match c {
            '\n' => {
                out.push_str("\\n");
                altered = true;
            }
            '\r' => {
                out.push_str("\\r");
                altered = true;
            }
            // ESC and the C1 controls: escape-sequence introducers.
            '\u{1b}' | '\u{80}'..='\u{9f}' => {
                out.push('\u{fffd}');
                altered = true;
            }
            // Bidi overrides, isolates and directional marks.
            '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{061c}' => {
                out.push('\u{fffd}');
                altered = true;
            }
            // Remaining C0 controls and DEL carry no display meaning here.
            c if c.is_control() => {
                altered = true;
            }
            c => out.push(c),
        }
    }

    // NFC alone can change the string without any character being escaped.
    if !altered && out != raw {
        altered = true;
    }

    let mut truncated = false;
    if out.chars().count() > cap {
        out = out.chars().take(cap.saturating_sub(1)).collect();
        out.push('…');
        truncated = true;
    }

    if out.is_empty() {
        out.push_str(EMPTY_MARKER);
    }

    Quarantined {
        text: out,
        altered,
        truncated,
    }
}

/// SSH-style fingerprint of a public key: `SHA256:` followed by the unpadded
/// base64 of the SHA-256 digest of the key's canonical prefixed encoding.
///
/// This is the one part of a bootstrap request the requester does not get to
/// choose, so it is the only field an approver can meaningfully compare
/// against a key they already trust. The digest is taken over the canonical
/// `algorithm:base64` form, which is how chaz prints keys everywhere else, so
/// the same key always yields the same fingerprint.
pub fn pubkey_fingerprint(pubkey: &PublicKey) -> String {
    let digest = Sha256::digest(pubkey.to_prefixed_string().as_bytes());
    let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest);
    format!("SHA256:{encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through_unchanged() {
        let q = quarantine("@chaz:example");
        assert_eq!(q.text, "@chaz:example");
        assert!(!q.altered);
        assert!(!q.truncated);
        assert_eq!(q.note(), "");
    }

    #[test]
    fn c0_controls_are_stripped() {
        let q = quarantine("ab\u{0}c\u{7}d\u{7f}");
        assert_eq!(q.text, "abcd");
        assert!(q.altered);
    }

    #[test]
    fn newlines_become_visible_escapes() {
        let q = quarantine("real\n  [2] forged request\r");
        assert_eq!(q.text, "real\\n  [2] forged request\\r");
        assert!(!q.text.contains('\n'));
        assert!(!q.text.contains('\r'));
        assert!(q.altered);
    }

    #[test]
    fn escape_and_osc_introducers_become_replacement_chars() {
        // A CSI colour change and an OSC window-title sequence.
        let q = quarantine("a\u{1b}[31mred\u{1b}]0;title\u{7}b");
        assert!(!q.text.contains('\u{1b}'));
        assert_eq!(q.text, "a\u{fffd}[31mred\u{fffd}]0;titleb");
        assert!(q.altered);
    }

    #[test]
    fn c1_single_byte_introducers_become_replacement_chars() {
        // U+009B is CSI on its own; U+009D is OSC.
        let q = quarantine("a\u{9b}31mb\u{9d}0;tc");
        assert_eq!(q.text, "a\u{fffd}31mb\u{fffd}0;tc");
        assert!(q.altered);
    }

    #[test]
    fn bidi_overrides_and_isolates_become_replacement_chars() {
        let q = quarantine("@chaz\u{202e}elpmaxe:\u{202c}");
        assert!(!q.text.contains('\u{202e}'));
        assert!(!q.text.contains('\u{202c}'));
        assert_eq!(q.text, "@chaz\u{fffd}elpmaxe:\u{fffd}");
        assert!(q.altered);

        let isolated = quarantine("a\u{2066}b\u{2069}c\u{200f}d\u{61c}e");
        assert_eq!(isolated.text, "a\u{fffd}b\u{fffd}c\u{fffd}d\u{fffd}e");
    }

    #[test]
    fn over_length_values_are_capped_and_marked() {
        let raw = "@".to_string() + &"a".repeat(200);
        let q = quarantine(&raw);
        assert_eq!(q.text.chars().count(), DISPLAY_CAP);
        assert!(q.text.ends_with('…'));
        assert!(q.truncated);
        assert_eq!(q.note(), "  (truncated)");
    }

    #[test]
    fn cap_counts_characters_not_bytes() {
        let raw = "é".repeat(60);
        let q = quarantine(&raw);
        assert_eq!(q.text.chars().count(), DISPLAY_CAP);
    }

    #[test]
    fn a_value_at_the_cap_is_not_truncated() {
        let raw = "a".repeat(DISPLAY_CAP);
        let q = quarantine(&raw);
        assert_eq!(q.text, raw);
        assert!(!q.truncated);
    }

    #[test]
    fn decomposed_text_is_normalized_to_nfc() {
        // "é" as e + U+0301.
        let q = quarantine("cafe\u{301}");
        assert_eq!(q.text, "café");
        assert!(q.altered);
    }

    #[test]
    fn empty_and_fully_stripped_values_render_as_a_marker() {
        assert_eq!(quarantine("").text, EMPTY_MARKER);
        assert_eq!(quarantine("\u{0}\u{1}").text, EMPTY_MARKER);
    }

    #[test]
    fn quarantined_text_never_contains_control_characters() {
        let hostile = "a\u{1b}[2J\nb\r\u{0}\u{9b}\u{202e}c";
        let q = quarantine(hostile);
        assert!(
            !q.text.chars().any(|c| c.is_control()),
            "control character survived: {:?}",
            q.text
        );
    }

    #[test]
    fn fingerprint_is_stable_and_ssh_shaped() {
        let key = eidetica::auth::crypto::generate_keypair().1;
        let fp = pubkey_fingerprint(&key);
        assert_eq!(fp, pubkey_fingerprint(&key));
        let b64 = fp.strip_prefix("SHA256:").expect("SHA256: prefix");
        // 32 bytes of digest, unpadded base64.
        assert_eq!(b64.len(), 43);
        assert!(!b64.contains('='));
    }

    #[test]
    fn different_keys_get_different_fingerprints() {
        let a = eidetica::auth::crypto::generate_keypair().1;
        let b = eidetica::auth::crypto::generate_keypair().1;
        assert_ne!(pubkey_fingerprint(&a), pubkey_fingerprint(&b));
    }
}
