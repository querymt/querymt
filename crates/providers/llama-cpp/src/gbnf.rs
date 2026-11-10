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
    /// Build the automaton for text that never contains `terminator`.
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

    /// State reached once the full terminator has been read (rejecting).
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
    /// Transitions out of `state` in collapsed form: the characters that
    /// must be excluded from the "back to state 0" class (every character
    /// whose transition is not to state 0, including the one that completes
    /// the terminator), and the explicit `(char, next_state)` edges for the
    /// characters that lead to a live state other than 0.
    ///
    /// Characters going back to state 0 are folded into the negated class,
    /// so each state has at most `1 + |edges|` alternatives. For a
    /// terminator starting with a character that does not repeat (`<`,
    /// `[`, ...) that is two or three alternatives per state, the same shape
    /// as the old `[^<]*`. llama.cpp's grammar engine forks one stack per
    /// alternative per character, so this directly bounds its cost.
    pub(crate) fn transitions(&self, state: usize) -> (Vec<char>, Vec<(char, usize)>) {
        let mut excluded = Vec::new();
        let mut edges = Vec::new();
        for &ch in &self.alphabet {
            let next = self.next(state, ch);
            if next == 0 {
                continue;
            }
            excluded.push(ch);
            if next != self.dead_state() {
                edges.push((ch, next));
            }
        }
        (excluded, edges)
    }

    /// Render the automaton as GBNF. `name` is the entry rule; the
    /// per-state rules are `{name}-0 ..`.
    pub(crate) fn rules(&self, name: &str) -> String {
        let mut out = format!("{name} ::= {name}-0\n");
        for state in 0..self.dead_state() {
            let (excluded, edges) = self.transitions(state);
            let class: String = excluded.iter().copied().map(class_member).collect();
            let _ = write!(out, "{name}-{state} ::= [^{class}] {name}-0");
            for (ch, next) in edges {
                let _ = write!(out, " | {} {name}-{next}", gbnf_literal(&ch.to_string()));
            }
            out.push_str(" | \"\"\n");
        }
        out
    }

    /// The pre-collapse rendering (one literal alternative per alphabet
    /// character in every state). Same language, many more parse stacks;
    /// kept only to benchmark against.
    #[cfg(test)]
    pub(crate) fn rules_uncollapsed(&self, name: &str) -> String {
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
             t-0 ::= [^a] t-0 | \"a\" t-1 | \"\"\n\
             t-1 ::= [^ab] t-0 | \"a\" t-1 | \"\"\n"
        );
    }

    #[test]
    fn escapes_class_members() {
        let rules = ExclusionDfa::new("<|\"|>").rules("q");
        // State 2 (`<|` seen): `"` advances, `<` restarts, the rest go home.
        assert!(rules.contains("q-2 ::= [^\\x22\\x3C] q-0"), "{rules}");
        assert!(rules.contains("\"\\\"\" q-3"), "{rules}");
    }

    /// Interpret rendered rules the way a GBNF engine would: each state's
    /// first alternative is `[^class] -> 0`, then literal edges. Checks the
    /// emitted grammar itself, not just `next()`.
    fn rendered_accepts(rules: &str, name: &str, text: &str) -> bool {
        use std::collections::HashMap;
        type State = (Vec<char>, Vec<(char, usize)>);
        let mut table: HashMap<usize, State> = HashMap::new();
        for line in rules.lines().skip(1) {
            let (lhs, rhs) = line.split_once(" ::= ").unwrap();
            let state: usize = lhs
                .strip_prefix(&format!("{name}-"))
                .unwrap()
                .parse()
                .unwrap();
            let alts: Vec<&str> = rhs.split(" | ").collect();
            let class = alts[0]
                .strip_prefix("[^")
                .and_then(|s| s.strip_suffix(&format!("] {name}-0")))
                .unwrap();
            let mut excluded = Vec::new();
            let mut rest = class;
            while !rest.is_empty() {
                if let Some(hex) = rest.strip_prefix("\\x") {
                    excluded.push(char::from(u8::from_str_radix(&hex[..2], 16).unwrap()));
                    rest = &hex[2..];
                } else {
                    let ch = rest.chars().next().unwrap();
                    excluded.push(ch);
                    rest = &rest[ch.len_utf8()..];
                }
            }
            let mut edges = Vec::new();
            for alt in &alts[1..] {
                if *alt == "\"\"" {
                    continue;
                }
                let (lit, target) = alt.rsplit_once(' ').unwrap();
                let lit = lit
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap()
                    .replace("\\\"", "\"");
                let target: usize = target
                    .strip_prefix(&format!("{name}-"))
                    .unwrap()
                    .parse()
                    .unwrap();
                edges.push((lit.chars().next().unwrap(), target));
            }
            table.insert(state, (excluded, edges));
        }
        let mut state = 0;
        for ch in text.chars() {
            let (excluded, edges) = &table[&state];
            state = if !excluded.contains(&ch) {
                0
            } else if let Some((_, next)) = edges.iter().find(|(c, _)| *c == ch) {
                *next
            } else {
                return false;
            };
        }
        true
    }

    #[test]
    fn value_cannot_contain_its_terminator() {
        for terminator in ["</parameter>", "<escape>", "<|\"|>", "aab", "abab", "a"] {
            let rules = ExclusionDfa::new(terminator).rules("t");
            let mut alphabet: Vec<char> = terminator.chars().collect();
            alphabet.sort_unstable();
            alphabet.dedup();
            alphabet.push('x');
            let max_len = if alphabet.len() > 6 { 5 } else { 8 };
            for text in all_strings(&alphabet, max_len) {
                assert_eq!(
                    rendered_accepts(&rules, "t", &text),
                    !text.contains(terminator),
                    "terminator={terminator:?} text={text:?}\n{rules}"
                );
            }
        }
    }

    /// llama.cpp forks one parse stack per alternative at every character,
    /// so the alternative count is the cost. State 0, where a value spends
    /// almost all its time, must branch like the old `[^<]*`.
    #[test]
    fn gbnf_text_values_stay_narrow() {
        let rules = ExclusionDfa::new("</parameter>").rules("v");
        let state0 = rules.lines().nth(1).unwrap();
        assert_eq!(state0, "v-0 ::= [^\\x3C] v-0 | \"<\" v-1 | \"\"", "{rules}");
        for line in rules.lines().skip(1) {
            // class, at most the next char and a restart on `<`, and "".
            assert!(line.split(" | ").count() <= 4, "{line}");
        }
    }
}
