use crate::anchors::store::LineAnchor;
use crate::hash::RapidHash;
use std::collections::{HashMap, HashSet};

const ALPHANUMERIC: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

pub(crate) fn split_lines_preserve_content(content: &str) -> Vec<&str> {
    content
        .split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line))
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .chain(if content.is_empty() { Some("") } else { None })
        .collect()
}

pub(crate) fn line_hashes(content: &str) -> Vec<RapidHash> {
    split_lines_preserve_content(content)
        .into_iter()
        .map(|line| RapidHash::new(line.as_bytes()))
        .collect()
}

/// Compute a per-file salt from the session ID and canonical path.
/// This ensures different sessions (or different files) produce
/// independent anchor sequences without any global mutable state.
pub(crate) fn file_salt(session_id: &str, path: &std::path::Path) -> u64 {
    let mut h: u64 = 0x9e37_79b9_7f4a_7c15; // golden-ratio derived constant
    for byte in session_id.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    for byte in path.to_string_lossy().as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x517c_c1b7_2722_0a95);
    }
    h
}

pub(crate) fn reconcile_lines(
    salt: u64,
    previous: Option<&[LineAnchor]>,
    current_hashes: &[RapidHash],
) -> Vec<LineAnchor> {
    let Some(previous) = previous else {
        return current_hashes
            .iter()
            .enumerate()
            .map(|(idx, line_hash)| LineAnchor {
                anchor: generate_anchor(salt, idx, line_hash.as_u64(), 0),
                line_hash: *line_hash,
            })
            .collect();
    };

    let mut previous_by_hash: HashMap<RapidHash, Vec<usize>> = HashMap::new();
    for (idx, line) in previous.iter().enumerate() {
        previous_by_hash
            .entry(line.line_hash)
            .or_default()
            .push(idx);
    }

    let mut used_previous = HashSet::new();
    let mut reconciled = Vec::with_capacity(current_hashes.len());

    for (current_idx, line_hash) in current_hashes.iter().enumerate() {
        let anchor = previous_by_hash
            .get(line_hash)
            .and_then(|candidates| choose_previous_anchor(candidates, current_idx, &used_previous))
            .map(|previous_idx| {
                used_previous.insert(previous_idx);
                previous[previous_idx].anchor.clone()
            })
            .unwrap_or_else(|| generate_anchor(salt, current_idx, line_hash.as_u64(), 0));

        reconciled.push(LineAnchor {
            anchor,
            line_hash: *line_hash,
        });
    }

    reconciled
}

fn choose_previous_anchor(
    candidates: &[usize],
    current_idx: usize,
    used_previous: &HashSet<usize>,
) -> Option<usize> {
    candidates
        .iter()
        .copied()
        .filter(|idx| !used_previous.contains(idx))
        .min_by_key(|idx| idx.abs_diff(current_idx))
}

/// Deterministic anchor generation from stable inputs.
///
/// Produces a visually diverse 6-character alphanumeric string derived from
/// the file salt, line index, and line hash.  No global mutable state required.
fn generate_anchor(salt: u64, line_index: usize, line_hash: u64, attempt: u64) -> String {
    // Mix all inputs via a simple but effective hash combiner.
    let mut h = salt;
    h ^= line_index as u64;
    h = h.wrapping_mul(0x517c_c1b7_2722_0a95);
    h ^= line_hash;
    h = h.wrapping_mul(0x6b7f_6243_9e4a_7c15);
    h ^= attempt;
    // Extra avalanching to ensure small input changes → large output changes.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;

    let mut chars = [0u8; 6];
    let mut v = h;
    for slot in &mut chars {
        *slot = ALPHANUMERIC[(v % 62) as usize];
        v /= 62;
    }

    // SAFETY: all bytes come from ALPHANUMERIC which is valid UTF-8.
    unsafe { std::str::from_utf8_unchecked(&chars) }.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(lines: &[&str]) -> Vec<RapidHash> {
        lines
            .iter()
            .map(|line| RapidHash::new(line.as_bytes()))
            .collect()
    }

    const TEST_SALT: u64 = 42;

    #[test]
    fn unchanged_lines_keep_anchors() {
        let first = reconcile_lines(TEST_SALT, None, &hashes(&["a", "b", "c"]));
        let second = reconcile_lines(TEST_SALT, Some(&first), &hashes(&["a", "b", "c"]));
        assert_eq!(
            first.iter().map(|line| &line.anchor).collect::<Vec<_>>(),
            second.iter().map(|line| &line.anchor).collect::<Vec<_>>()
        );
    }

    #[test]
    fn insertion_above_preserves_existing_anchors() {
        let first = reconcile_lines(TEST_SALT, None, &hashes(&["a", "b", "c"]));
        let second = reconcile_lines(TEST_SALT, Some(&first), &hashes(&["new", "a", "b", "c"]));

        assert_ne!(second[0].anchor, first[0].anchor);
        assert_eq!(second[1].anchor, first[0].anchor);
        assert_eq!(second[2].anchor, first[1].anchor);
        assert_eq!(second[3].anchor, first[2].anchor);
    }

    #[test]
    fn duplicate_identical_lines_receive_distinct_anchors() {
        let first = reconcile_lines(TEST_SALT, None, &hashes(&["}", "}", "}"]));
        let anchors: HashSet<_> = first.iter().map(|line| line.anchor.as_str()).collect();
        assert_eq!(anchors.len(), 3);
    }

    #[test]
    fn deleted_lines_retire_anchors() {
        let first = reconcile_lines(TEST_SALT, None, &hashes(&["a", "b", "c"]));
        let second = reconcile_lines(TEST_SALT, Some(&first), &hashes(&["a", "c"]));

        assert_eq!(second[0].anchor, first[0].anchor);
        assert_eq!(second[1].anchor, first[2].anchor);
        assert!(!second.iter().any(|line| line.anchor == first[1].anchor));
    }

    #[test]
    fn splits_lf_crlf_and_no_trailing_newline() {
        assert_eq!(split_lines_preserve_content("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines_preserve_content("a\r\nb\r\n"), vec!["a", "b"]);
        assert_eq!(split_lines_preserve_content("a\nb"), vec!["a", "b"]);
    }

    #[test]
    fn different_salts_produce_different_anchors() {
        let a = reconcile_lines(1, None, &hashes(&["hello"]));
        let b = reconcile_lines(2, None, &hashes(&["hello"]));
        assert_ne!(a[0].anchor, b[0].anchor);
    }

    #[test]
    fn anchors_are_diverse_no_aaaa_prefix() {
        let lines = reconcile_lines(TEST_SALT, None, &hashes(&["a", "b", "c", "d", "e"]));
        for line in &lines {
            // Anchors should not all share a common 4-char prefix.
            assert_eq!(line.anchor.len(), 6);
        }
        // At least 3 distinct first characters among 5 anchors.
        let first_chars: HashSet<char> = lines
            .iter()
            .map(|l| l.anchor.chars().next().unwrap())
            .collect();
        assert!(
            first_chars.len() >= 3,
            "anchors too similar: {:?}",
            lines.iter().map(|l| &l.anchor).collect::<Vec<_>>()
        );
    }
}
