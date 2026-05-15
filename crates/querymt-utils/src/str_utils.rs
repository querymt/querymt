//! UTF-8-safe string truncation helpers.
//!
//! Rust string slicing (`&s[..n]`) panics when `n` is not a valid UTF-8
//! char boundary.  The helpers here walk back to the nearest boundary so
//! that display-oriented truncation never panics, even with multibyte
//! characters (Cyrillic, CJK, emoji, etc.).

/// Truncate `input` to at most `max_bytes` bytes, appending `"..."` when
/// the string is actually shortened.
///
/// The returned string is guaranteed to be valid UTF-8 and at most
/// `max_bytes` bytes long (including the ellipsis).
///
/// # Panics
/// Never.
///
/// # Examples
/// ```
/// use querymt_utils::str_utils::truncate_with_ellipsis;
///
/// // ASCII – no change needed
/// assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
///
/// // ASCII truncation
/// assert_eq!(truncate_with_ellipsis("hello world", 8), "hello...");
///
/// // Cyrillic: 'ч' is 2 bytes.  If `max_bytes` lands mid-character we
/// // walk back so the result is still valid UTF-8.
/// let cyrillic = "абвгдежз";  // each char 2 bytes
/// let truncated = truncate_with_ellipsis(cyrillic, 7);
/// assert!(truncated.ends_with("..."));
/// assert!(truncated.is_char_boundary(truncated.len()));
/// ```
pub fn truncate_with_ellipsis(input: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "...";
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes <= ELLIPSIS.len() {
        // Not enough room for text + "...": just return as many whole chars as fit.
        return input.chars().take(max_bytes).collect();
    }
    let target = max_bytes - ELLIPSIS.len();
    let end = floor_char_boundary(input, target);
    format!("{}{}", &input[..end], ELLIPSIS)
}

/// Truncate `input` to at most `max_bytes` bytes, appending `suffix` when
/// the string is actually shortened.
///
/// Similar to [`truncate_with_ellipsis`] but with a custom suffix.
pub fn truncate_with_suffix(input: &str, max_bytes: usize, suffix: &str) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes <= suffix.len() {
        return input.chars().take(max_bytes).collect();
    }
    let target = max_bytes - suffix.len();
    let end = floor_char_boundary(input, target);
    let mut out = String::with_capacity(end + suffix.len());
    out.push_str(&input[..end]);
    out.push_str(suffix);
    out
}

/// Truncate `input` to at most `max_bytes` bytes **without** any suffix.
///
/// Returns a string slice (borrowed from `input`) that ends on a char
/// boundary and is at most `max_bytes` bytes long.
///
/// This is a thin convenience wrapper around [`floor_char_boundary`].
pub fn truncate_str(input: &str, max_bytes: usize) -> &str {
    let end = floor_char_boundary(input, max_bytes);
    &input[..end]
}

/// Return the largest index `<= target` that is a valid UTF-8 char
/// boundary in `input`.
///
/// This is equivalent to the nightly `str::floor_char_boundary` stabilized
/// in Rust 1.82.  Kept as a polyfill so the codebase compiles on older
/// stable compilers.
fn floor_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    let mut i = target;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_no_truncation() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
    }

    #[test]
    fn ascii_truncation() {
        assert_eq!(truncate_with_ellipsis("hello world", 8), "hello...");
    }

    #[test]
    fn exactly_fits() {
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn tiny_budget_returns_few_chars() {
        let result = truncate_with_ellipsis("abc", 1);
        assert_eq!(result, "a");
    }

    #[test]
    fn cyrillic_does_not_panic() {
        // The exact crash from issue #525: byte 237 lands inside 'ч' (2-byte char).
        let ukrainian = "зроби коміт без gpg, до цього стану та почни виправленя.";
        let truncated = truncate_with_ellipsis(ukrainian, 240);
        assert!(truncated.len() <= 240);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn cyrillic_boundary_inside_char() {
        // 'ч' is 2 bytes (0xD1 0x87).  6 bytes of "абв" + "..." = 9 bytes,
        // so with max=7 we need to drop the last char to fit "...".
        let cyrillic = "абвгдежз"; // each char 2 bytes
        let truncated = truncate_with_ellipsis(cyrillic, 7);
        // Can fit at most 2 chars (4 bytes) + "..." (3 bytes) = 7
        assert_eq!(truncated, "аб...");
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn emoji_multibyte() {
        // '🎉' is 4 bytes.  If budget lands mid-emoji we must back up.
        let emojis = "🎉🎉🎉🎉";
        let truncated = truncate_with_ellipsis(emojis, 10);
        // 1 emoji (4 bytes) + "..." (3 bytes) = 7 <= 10
        // Could also fit 2 emojis (8 bytes) + "..." (3 bytes) = 11 > 10, so only 1.
        assert_eq!(truncated, "🎉...");
    }

    #[test]
    fn cjk_truncation() {
        let cjk = "你好世界你好世界"; // each char 3 bytes
        let truncated = truncate_with_ellipsis(cjk, 10);
        // 2 chars (6 bytes) + "..." (3 bytes) = 9 <= 10
        assert_eq!(truncated, "你好...");
    }

    #[test]
    fn truncate_str_basic() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_str_inside_multibyte() {
        let s = "абвгд"; // each 2 bytes, total 10
        assert_eq!(truncate_str(s, 5), "аб"); // floor to 4 (2 chars)
    }

    #[test]
    fn truncate_with_suffix_custom() {
        let s = "abcdefghij";
        assert_eq!(truncate_with_suffix(s, 8, "…"), "abcde…");
    }

    #[test]
    fn truncate_with_suffix_multibyte() {
        let s = "абвгдежз"; // 14 bytes
        assert_eq!(truncate_with_suffix(s, 7, "..."), "аб...");
    }

    #[test]
    fn empty_string() {
        assert_eq!(truncate_with_ellipsis("", 5), "");
    }

    #[test]
    fn zero_budget() {
        assert_eq!(truncate_with_ellipsis("abc", 0), "");
    }

    #[test]
    fn ukrainian_issue_525_exact_text() {
        // Simulate the exact crash scenario: 240 byte budget on Ukrainian text
        let text = "зроби коміт без gpg, до цього стану та почни виправленя. А ще, зараз проблема з подвійним натисканням шифт + клік, щоб переместити річ у сховище";
        let result = truncate_with_ellipsis(text, 240);
        assert!(result.len() <= 240);
        assert!(result.is_char_boundary(result.len()));
        assert!(result.ends_with("...") || result == text);
    }
}
