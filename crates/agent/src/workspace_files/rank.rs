//! Fuzzy ranking of workspace paths (frizbee, SIMD Smith-Waterman).
//!
//! Kept in one module so the pre-1.0 `frizbee` API is isolated.
//!
//! 1. *Path pass*: the needle against every relative path.
//! 2. *Basename pass*: the same needle against the basenames of the path-pass
//!    matches only.
//! 3. Score is the saturating sum; ties break on `exact`, shallower depth,
//!    shorter path, then lexicographic order, so output is deterministic.

use frizbee::{Config, Matcher};
use std::cmp::Ordering;

/// Longest query considered; keeps scores inside frizbee's `u16` range.
const MAX_QUERY_CHARS: usize = 256;

/// Outcome of [`rank`]: candidate indices, best first.
#[derive(Debug, PartialEq, Eq)]
pub struct Ranking {
    pub order: Vec<usize>,
    /// More candidates matched than `limit`.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy)]
struct Scored {
    index: usize,
    score: u16,
    exact: bool,
}

/// Rank `paths` (relative, `/`-separated) against `query`, returning at most
/// `limit` indices. An empty query lists shallow entries first.
pub fn rank<S: AsRef<str>>(paths: &[S], query: &str, limit: usize) -> Ranking {
    let query: String = query.trim().chars().take(MAX_QUERY_CHARS).collect();
    if query.is_empty() {
        return list_shallow_first(paths, limit);
    }

    let max_typos = u16::try_from(query.chars().count() / 4).unwrap_or(u16::MAX);
    let config = Config::default().max_typos(Some(max_typos));
    let mut matcher = Matcher::new(query.as_str(), &config);

    let path_matches = matcher.match_list(paths);
    let basenames: Vec<&str> = path_matches
        .iter()
        .map(|m| basename(paths[m.index as usize].as_ref()))
        .collect();
    let mut basename_score: Vec<(u16, bool)> = vec![(0, false); path_matches.len()];
    for m in matcher.match_list(&basenames) {
        basename_score[m.index as usize] = (m.score, m.exact);
    }

    let mut scored: Vec<Scored> = path_matches
        .iter()
        .zip(&basename_score)
        .map(|(path, &(base_score, base_exact))| Scored {
            index: path.index as usize,
            score: path.score.saturating_add(base_score),
            exact: path.exact || base_exact,
        })
        .collect();

    let by_quality = |a: &Scored, b: &Scored| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.exact.cmp(&a.exact))
            .then_with(|| shallow_first(paths[a.index].as_ref(), paths[b.index].as_ref()))
    };
    let truncated = scored.len() > limit;
    top_n(&mut scored, limit, by_quality);
    Ranking {
        order: scored.into_iter().map(|s| s.index).collect(),
        truncated,
    }
}

fn list_shallow_first<S: AsRef<str>>(paths: &[S], limit: usize) -> Ranking {
    let mut order: Vec<usize> = (0..paths.len()).collect();
    let truncated = order.len() > limit;
    top_n(&mut order, limit, |&a, &b| {
        shallow_first(paths[a].as_ref(), paths[b].as_ref())
    });
    Ranking { order, truncated }
}

/// Keep only the `n` smallest items under `cmp`, sorted. Selecting first makes
/// this linear in the input instead of `O(len log len)`.
fn top_n<T>(items: &mut Vec<T>, n: usize, mut cmp: impl FnMut(&T, &T) -> Ordering) {
    if items.len() > n {
        if n > 0 {
            items.select_nth_unstable_by(n - 1, &mut cmp);
        }
        items.truncate(n);
    }
    items.sort_unstable_by(cmp);
}

/// Shallower depth, then shorter path, then lexicographic.
fn shallow_first(a: &str, b: &str) -> Ordering {
    depth(a)
        .cmp(&depth(b))
        .then_with(|| a.len().cmp(&b.len()))
        .then_with(|| a.cmp(b))
}

fn depth(path: &str) -> usize {
    path.matches('/').count()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranked<'a>(paths: &'a [&'a str], query: &str, limit: usize) -> Vec<&'a str> {
        rank(paths, query, limit)
            .order
            .into_iter()
            .map(|i| paths[i])
            .collect()
    }

    #[test]
    fn basename_match_outranks_scattered_match() {
        let paths = ["src/m/o/d/x.rs", "src/model.rs"];
        assert_eq!(
            ranked(&paths, "mod", 10),
            ["src/model.rs", "src/m/o/d/x.rs"]
        );
    }

    #[test]
    fn segment_start_gets_a_bonus() {
        let paths = ["srcb/x.rs", "src/b/x.rs"];
        assert_eq!(ranked(&paths, "b", 10)[0], "src/b/x.rs");
    }

    #[test]
    fn output_is_identical_across_runs() {
        let paths = [
            "a/mod.rs",
            "b/mod.rs",
            "mod.rs",
            "src/model.rs",
            "src/modules/a.rs",
            "x/y/mod.rs",
        ];
        let first = rank(&paths, "mod", 4);
        for _ in 0..20 {
            assert_eq!(rank(&paths, "mod", 4), first);
        }
    }

    #[test]
    fn empty_query_lists_shallow_entries_first() {
        let paths = ["a/b/c.rs", "z.rs", "a/b.rs", "a"];
        assert_eq!(ranked(&paths, "", 10), ["a", "z.rs", "a/b.rs", "a/b/c.rs"]);
    }

    #[test]
    fn short_queries_must_match_exactly_as_subsequence() {
        let paths = ["abc.rs", "axc.rs"];
        assert_eq!(ranked(&paths, "abc", 10), ["abc.rs"]);
    }

    #[test]
    fn longer_queries_tolerate_one_typo_per_four_chars() {
        let paths = ["config.rs"];
        assert_eq!(ranked(&paths, "confxg.rs", 10), ["config.rs"]);
    }

    #[test]
    fn truncation_is_reported() {
        let paths = ["a1", "a2", "a3"];
        let r = rank(&paths, "a", 2);
        assert_eq!(r.order.len(), 2);
        assert!(r.truncated);
        assert!(!rank(&paths, "a", 3).truncated);
    }

    #[test]
    fn empty_query_truncation_is_reported() {
        let paths = ["a", "b", "c"];
        let r = rank(&paths, "  ", 1);
        assert_eq!(r.order, [0]);
        assert!(r.truncated);
    }

    #[test]
    fn no_match_returns_empty() {
        let paths = ["a.rs"];
        let r = rank(&paths, "zzz", 10);
        assert!(r.order.is_empty());
        assert!(!r.truncated);
    }
}
