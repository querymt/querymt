//! Helpers for building GBNF grammars.

use std::fmt::Write as _;

/// Render `value` as a double-quoted GBNF string literal.
pub(crate) fn gbnf_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Render `ch` as a member of a GBNF character class.
fn class_member(ch: char) -> String {
    if ch.is_ascii_alphanumeric() {
        ch.to_string()
    } else if (ch as u32) < 0x80 {
        format!("\\x{:02X}", ch as u32)
    } else {
        format!("\\U{:08X}", ch as u32)
    }
}

/// Automaton for "any text that does not contain `terminator`".
///
/// State `k` means the last `k` characters seen are the first `k` characters
/// of the terminator (the longest such `k`). Reaching state `terminator.len()`
/// is forbidden.
///
/// Unlike `[^<]*` this lets every character through, including a lone `<`,
/// and only rejects the exact closing sequence. The grammar that follows the
/// value supplies the real terminator, so the two never overlap.
#[derive(Debug, Clone)]
pub(crate) struct ExclusionDfa {
    terminator: Vec<char>,
    alphabet: Vec<char>,
}

impl ExclusionDfa {
    pub(crate) fn new(terminator: &str) -> Self {
        let terminator: Vec<char> = terminator.chars().collect();
        assert!(!terminator.is_empty(), "terminator must not be empty");
        let mut alphabet = terminator.clone();
        alphabet.sort_unstable();
        alphabet.dedup();
        Self {
            terminator,
            alphabet,
        }
    }

    fn dead_state(&self) -> usize {
        self.terminator.len()
    }

    /// Next state after reading `ch` in `state`. Returns `dead_state()` when
    /// the terminator has just been completed.
    pub(crate) fn next(&self, state: usize, ch: char) -> usize {
        let mut seen: Vec<char> = self.terminator[..state].to_vec();
        seen.push(ch);
        (0..=seen.len().min(self.terminator.len()))
            .rev()
            .find(|&len| seen[seen.len() - len..] == self.terminator[..len])
            .unwrap_or(0)
    }

    /// Whether `text` is accepted, i.e. does not contain the terminator.
    #[cfg(test)]
    pub(crate) fn accepts(&self, text: &str) -> bool {
        let mut state = 0;
        for ch in text.chars() {
            state = self.next(state, ch);
            if state == self.dead_state() {
                return false;
            }
        }
        true
    }

    /// Render the automaton as GBNF. `name` is the entry rule; the
    /// per-state rules are `{name}-0 ..`.
    pub(crate) fn rules(&self, name: &str) -> String {
        let class: String = self.alphabet.iter().copied().map(class_member).collect();
        let mut out = format!("{name} ::= {name}-0\n");
        for state in 0..self.dead_state() {
            let _ = write!(out, "{name}-{state} ::= [^{class}] {name}-0");
            for &ch in &self.alphabet {
                let next = self.next(state, ch);
                if next != self.dead_state() {
                    let _ = write!(out, " | {} {name}-{next}", gbnf_literal(&ch.to_string()));
                }
            }
            out.push_str(" | \"\"\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every string over a small alphabet up to `max_len` characters.
    fn all_strings(alphabet: &[char], max_len: usize) -> Vec<String> {
        let mut all = vec![String::new()];
        let mut layer = vec![String::new()];
        for _ in 0..max_len {
            layer = layer
                .iter()
                .flat_map(|prefix| alphabet.iter().map(move |c| format!("{prefix}{c}")))
                .collect();
            all.extend(layer.iter().cloned());
        }
        all
    }

    #[test]
    fn dfa_matches_substring_exclusion_exhaustively() {
        for terminator in ["</parameter>", "<escape>", "<|\"|>", "aab", "abab", "a"] {
            let dfa = ExclusionDfa::new(terminator);
            let mut alphabet: Vec<char> = terminator.chars().collect();
            alphabet.sort_unstable();
            alphabet.dedup();
            alphabet.push('x');
            // Keep the search space small for large alphabets.
            let max_len = if alphabet.len() > 6 { 5 } else { 8 };
            for text in all_strings(&alphabet, max_len) {
                assert_eq!(
                    dfa.accepts(&text),
                    !text.contains(terminator),
                    "terminator={terminator:?} text={text:?}"
                );
            }
        }
    }

    #[test]
    fn dfa_accepts_comparison_operators_and_markup() {
        let dfa = ExclusionDfa::new("</parameter>");
        for ok in [
            "",
            "a < b",
            "x <= y",
            "<<",
            "<",
            "a <",
            "<div></div>",
            "</param",
            "</parameter",
            "</parameters",
            "if row[j] < best_d:\n    pass\n",
        ] {
            assert!(dfa.accepts(ok), "should accept {ok:?}");
        }
        for bad in ["</parameter>", "x</parameter>y", "<</parameter>"] {
            assert!(!dfa.accepts(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn renders_small_automaton() {
        let rules = ExclusionDfa::new("ab").rules("t");
        assert_eq!(
            rules,
            "t ::= t-0\n\
             t-0 ::= [^ab] t-0 | \"a\" t-1 | \"b\" t-0 | \"\"\n\
             t-1 ::= [^ab] t-0 | \"a\" t-1 | \"\"\n"
        );
    }

    #[test]
    fn escapes_class_members() {
        let rules = ExclusionDfa::new("<|\"|>").rules("q");
        assert!(rules.contains("[^\\x22\\x3C\\x3E\\x7C]"), "{rules}");
        assert!(rules.contains("\"\\\"\""), "{rules}");
    }
}
