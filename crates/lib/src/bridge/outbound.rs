//! Shared outbound support for transport bridge implementations and user interaction.
//!
//! Bridges own their transport-specific message limits and error types. This module
//! provides the common chunking and bounded retry policy used to deliver replies.

/// Maximum attempts for one outbound transport chunk, including the first.
pub const OUTBOUND_CHUNK_MAX_ATTEMPTS: u32 = 3;
const OUTBOUND_CHUNK_BACKOFF_BASE: std::time::Duration = std::time::Duration::from_millis(500);
const OUTBOUND_CHUNK_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// HTTP statuses for which an outbound transport chunk may be retried.
pub fn is_transient_outbound_status(status: u16) -> bool {
    matches!(status, 429 | 503 | 504)
}

/// Split a body into chunks at most `limit` Unicode scalar values long.
///
/// Concatenating the chunks reproduces `body` exactly, byte for byte.
/// Splitting prefers newlines, then spaces, and hard-splits an oversized
/// token only when no boundary fits. Empty bodies yield no chunks.
/// Whitespace-only chunks occur only when the remaining input is all whitespace.
pub fn chunk_message_chars(body: &str, limit: usize) -> Vec<String> {
    chunk_message(body, limit, |_| 1)
}

/// Split a body into chunks at most `limit` UTF-8 bytes long.
///
/// Concatenating the chunks reproduces `body` exactly, byte for byte.
/// Splitting prefers newlines, then spaces, and hard-splits an oversized
/// token only when no boundary fits. Empty bodies yield no chunks. A limit
/// smaller than a scalar's UTF-8 encoding cannot produce a valid chunk.
/// Whitespace-only chunks occur only when the remaining input is all whitespace.
pub fn chunk_message_bytes(body: &str, limit: usize) -> Vec<String> {
    chunk_message(body, limit, char::len_utf8)
}

fn chunk_message(body: &str, limit: usize, measure_char: fn(char) -> usize) -> Vec<String> {
    assert!(limit > 0, "chunk limit must be positive");
    let mut chunks = Vec::new();
    let mut rest = body;

    while !rest.is_empty() {
        let mut used = 0;
        let mut prefix_end = 0;
        for (index, ch) in rest.char_indices() {
            let width = measure_char(ch);
            if used + width > limit {
                break;
            }
            used += width;
            prefix_end = index + ch.len_utf8();
        }

        assert!(prefix_end > 0, "chunk limit cannot fit a Unicode scalar");
        let prefix = &rest[..prefix_end];
        let boundary = (prefix_end < rest.len())
            .then(|| {
                prefix
                    .rfind('\n')
                    .or_else(|| prefix.rfind(' '))
                    .map(|index| index + 1)
                    .filter(|&end| !prefix[..end].trim().is_empty())
            })
            .flatten();
        let end = boundary.unwrap_or(prefix_end);
        chunks.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    chunks
}

/// Hard-split a token into consecutive character-safe chunks of at most
/// `limit` Unicode scalar values.
pub fn hard_split_chars(token: &str, limit: usize) -> Vec<String> {
    hard_split(token, limit, |_| 1)
}

fn hard_split(token: &str, limit: usize, measure_char: fn(char) -> usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0;
    for ch in token.chars() {
        let char_len = measure_char(ch);
        if cur_len + char_len > limit && !cur.is_empty() {
            pieces.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur.push(ch);
        cur_len += char_len;
    }
    if !cur.is_empty() {
        pieces.push(cur);
    }
    pieces
}

/// Retry a single outbound transport chunk under the shared bridge policy.
///
/// The caller supplies transport-specific transient-error classification; the
/// bound, exponential backoff, and jitter stay identical across transports.
/// A successful chunk is never sent again while a later chunk is retried.
pub async fn retry_outbound_chunk<F, Fut, E, Classify>(
    send: F,
    is_transient: Classify,
) -> Result<(), E>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    Classify: Fn(&E) -> bool,
{
    retry_outbound_chunk_with_backoff(send, is_transient, |delay| async move {
        tokio::time::sleep(delay).await;
    })
    .await
}

async fn retry_outbound_chunk_with_backoff<F, Fut, E, Classify, Wait, WaitFut>(
    send: F,
    is_transient: Classify,
    wait: Wait,
) -> Result<(), E>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    Classify: Fn(&E) -> bool,
    Wait: Fn(std::time::Duration) -> WaitFut,
    WaitFut: std::future::Future<Output = ()>,
{
    for attempt in 1..=OUTBOUND_CHUNK_MAX_ATTEMPTS {
        match send().await {
            Ok(()) => return Ok(()),
            Err(error) if is_transient(&error) && attempt < OUTBOUND_CHUNK_MAX_ATTEMPTS => {
                wait(outbound_chunk_backoff(attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the final attempt always returns")
}

fn outbound_chunk_backoff(retry_number: u32) -> std::time::Duration {
    let multiplier = 1u128 << retry_number.saturating_sub(1);
    let base_ms = OUTBOUND_CHUNK_BACKOFF_BASE.as_millis();
    let capped_ms = base_ms
        .saturating_mul(multiplier)
        .min(OUTBOUND_CHUNK_BACKOFF_CAP.as_millis());
    // getrandom failure must not turn a transient delivery problem into a
    // permanent one; an unjittered bounded delay is the safe fallback.
    let jitter_percent = getrandom::u32().map(|n| n % 21).unwrap_or(10);
    std::time::Duration::from_millis((capped_ms * (90 + u128::from(jitter_percent)) / 100) as u64)
}

#[cfg(test)]
mod tests {
    use super::{
        OUTBOUND_CHUNK_MAX_ATTEMPTS, chunk_message_bytes, chunk_message_chars as chunk_message,
        hard_split_chars as hard_split, is_transient_outbound_status,
        retry_outbound_chunk_with_backoff,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn outbound_chunk_retries_transient_failures_independently() {
        let attempts = Arc::new(Mutex::new(HashMap::<String, u32>::new()));
        let waits = Arc::new(Mutex::new(Vec::new()));

        for chunk in ["first", "second"] {
            let attempts = attempts.clone();
            let waits = waits.clone();
            retry_outbound_chunk_with_backoff(
                || {
                    let attempts = attempts.clone();
                    async move {
                        let mut attempts = attempts.lock().await;
                        let count = attempts.entry(chunk.to_string()).or_default();
                        *count += 1;
                        if *count == 1 {
                            Err("transient")
                        } else {
                            Ok(())
                        }
                    }
                },
                |error| *error == "transient",
                |delay| {
                    let waits = waits.clone();
                    async move { waits.lock().await.push(delay) }
                },
            )
            .await
            .expect("each chunk retries its own first failure");
        }

        assert_eq!(
            *attempts.lock().await,
            HashMap::from([("first".into(), 2), ("second".into(), 2)])
        );
        let waits = waits.lock().await;
        assert_eq!(waits.len(), 2);
        assert!(
            waits
                .iter()
                .all(|delay| (450..=550).contains(&delay.as_millis()))
        );
    }

    #[tokio::test]
    async fn outbound_chunk_stops_after_three_transient_attempts_and_never_retries_permanent() {
        let transient_attempts = Arc::new(AtomicU64::new(0));
        let result = retry_outbound_chunk_with_backoff(
            || {
                let attempts = transient_attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>("transient")
                }
            },
            |error| *error == "transient",
            |_| async {},
        )
        .await;
        assert_eq!(result, Err("transient"));
        assert_eq!(
            transient_attempts.load(Ordering::SeqCst),
            u64::from(OUTBOUND_CHUNK_MAX_ATTEMPTS)
        );

        let permanent_attempts = Arc::new(AtomicU64::new(0));
        let result = retry_outbound_chunk_with_backoff(
            || {
                let attempts = permanent_attempts.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>("permanent")
                }
            },
            |error| *error == "transient",
            |_| async {},
        )
        .await;
        assert_eq!(result, Err("permanent"));
        assert_eq!(permanent_attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn only_explicit_outbound_statuses_are_transient() {
        for status in [429, 503, 504] {
            assert!(is_transient_outbound_status(status));
        }
        for status in [400, 401, 403, 404, 500, 502] {
            assert!(!is_transient_outbound_status(status));
        }
    }

    /// Every chunk must respect the char ceiling — the whole point of the split.
    fn assert_within_limit(chunks: &[String], limit: usize) {
        for c in chunks {
            assert!(
                c.chars().count() <= limit,
                "chunk exceeds limit {limit}: {} chars",
                c.chars().count()
            );
        }
    }

    #[test]
    fn short_body_is_one_chunk_unchanged() {
        assert_eq!(chunk_message("hello world", 2000), vec!["hello world"]);
    }

    #[test]
    fn body_at_exactly_the_limit_is_not_split() {
        let body = "x".repeat(2000);
        let chunks = chunk_message(&body, 2000);
        assert_eq!(chunks, vec![body]);
    }

    #[test]
    fn empty_body_sends_nothing() {
        assert!(chunk_message("", 2000).is_empty());
    }

    fn assert_lossless_chunks(body: &str, limit: usize) {
        let char_chunks = chunk_message(body, limit);
        assert_within_limit(&char_chunks, limit);
        assert_eq!(char_chunks.concat(), body);

        if body.chars().all(|ch| ch.len_utf8() <= limit) {
            let byte_chunks = chunk_message_bytes(body, limit);
            assert!(byte_chunks.iter().all(|chunk| chunk.len() <= limit));
            assert_eq!(byte_chunks.concat(), body);
        }
    }

    #[test]
    fn chunks_preserve_every_byte_across_boundaries() {
        for limit in 1..=12 {
            for body in [
                "aaaaa bbbbb",
                "aaaaa\nbbbbb",
                "a  b",
                "short supercalifragilisticexpialidocious",
                "🦀 a\n🦀🦀 b",
            ] {
                assert_lossless_chunks(body, limit);
            }
        }
    }

    #[test]
    fn splits_on_newline_boundaries() {
        // Three 40-char lines, limit 100: two lines pack, the third spills over.
        let line = "a".repeat(40);
        let body = format!("{line}\n{line}\n{line}");
        let chunks = chunk_message(&body, 100);
        assert_within_limit(&chunks, 100);
        // First chunk packs two lines joined by the newline; never a torn line.
        assert_eq!(chunks, vec![format!("{line}\n{line}\n"), line]);
        assert_eq!(chunks.concat(), body);
    }

    #[test]
    fn splits_long_line_on_spaces_without_breaking_words() {
        // One line of 10 six-char words ("word00".."word09") = 69 chars.
        let words: Vec<String> = (0..10).map(|i| format!("word{i:02}")).collect();
        let body = words.join(" ");
        let chunks = chunk_message(&body, 20);
        assert_within_limit(&chunks, 20);
        // No word is ever split: every whitespace-delimited token in every chunk
        // is one of the originals.
        for chunk in &chunks {
            for tok in chunk.split(' ').filter(|tok| !tok.is_empty()) {
                assert!(words.contains(&tok.to_string()), "torn word: {tok:?}");
            }
        }
        assert_eq!(chunks.concat(), body);
    }

    #[test]
    fn hard_splits_a_single_oversized_token() {
        // A 4500-char URL-like blob with no spaces: only a hard cut fits it.
        let blob = "h".repeat(4500);
        let chunks = chunk_message(&blob, 2000);
        assert_within_limit(&chunks, 2000);
        assert_eq!(chunks.len(), 3); // 2000 + 2000 + 500
        assert_eq!(chunks.concat(), blob); // no character lost or duplicated
    }

    #[test]
    fn oversized_token_tail_packs_with_following_words() {
        // A 25-char token then a short word, limit 10: the token hard-splits to
        // [10, 10, 5] and the trailing "hi" packs onto the open 5-char tail
        // rather than stranding on its own message.
        let body = format!("{} hi", "z".repeat(25));
        let chunks = chunk_message(&body, 10);
        assert_within_limit(&chunks, 10);
        assert_eq!(chunks, vec!["zzzzzzzzzz", "zzzzzzzzzz", "zzzzz hi"]);
        assert_eq!(chunks.concat(), body);
    }

    #[test]
    fn cuts_never_fall_inside_a_multibyte_char() {
        // Each char is multi-byte; a byte-indexed split would panic or corrupt.
        let body = "áéíóú".repeat(3); // 15 chars, 30 bytes
        let chunks = chunk_message(&body, 4);
        assert_within_limit(&chunks, 4);
        assert_eq!(chunks.concat(), body); // round-trips intact
        // Sanity: a 4-byte emoji is one char, so it packs four-per-chunk.
        let crabs = "🦀".repeat(9);
        let chunks = chunk_message(&crabs, 4);
        assert_within_limit(&chunks, 4);
        assert_eq!(chunks.concat(), crabs);
    }

    #[test]
    fn hard_split_is_exact_and_lossless() {
        assert_eq!(hard_split("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(hard_split("ab", 3), vec!["ab"]);
        assert!(hard_split("", 3).is_empty());
    }

    #[test]
    fn mixed_body_every_chunk_within_limit() {
        // Newlines, normal words, a giant token, and blank lines together.
        let body = format!(
            "intro line here\n\n{}\nshort tail\n{}",
            "tok ".repeat(200).trim(),
            "Q".repeat(3000)
        );
        let chunks = chunk_message(&body, 2000);
        assert_within_limit(&chunks, 2000);
        assert!(!chunks.is_empty());
    }

    #[test]
    fn byte_chunks_prefer_boundaries_and_hard_split_on_char_boundaries() {
        let body = "alpha\nbravo\ncharlie";
        let chunks = chunk_message_bytes(body, 11);
        assert_eq!(chunks, vec!["alpha\n", "bravo\n", "charlie"]);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 11));
        assert_eq!(chunks.concat(), body);

        let crabs = "🦀".repeat(3);
        let chunks = chunk_message_bytes(&crabs, 8);
        assert_eq!(chunks, vec!["🦀🦀", "🦀"]);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 8));
        assert_eq!(chunks.concat(), crabs);
    }
}
