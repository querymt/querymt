//! One fuzzy matcher for the app's search boxes (model picker, profile and
//! target selects, workspace field, slash command lists), on `frizbee`.
//!
//! A query is split into words. Every word must match inside one of the
//! item's fields, in order with gaps allowed; different words may match in
//! different fields. Fields come in two tiers (`rank_tiered`): a match in a
//! secondary field (provider, path, description) counts for half of the same
//! match in the item's own name. Separators (`-` `_` `.` `/` `:`) count as
//! spaces, case is ignored, words of five or more characters tolerate about
//! one typo in four, and operators such as `^` or `!` are matched literally.

use frizbee::{Config, Matcher, Pattern, PatternConfig};

/// Lowercase `text` and turn the separators that split names and versions
/// into spaces.
fn normalize(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '-' | '_' | '.' | '/' | ':' => ' ',
            c => c,
        })
        .collect()
}

/// The items' field at one position: who owns each haystack, the haystack,
/// and whether it is a secondary field.
#[derive(Default)]
struct Column<'a> {
    owners: Vec<usize>,
    haystacks: Vec<&'a str>,
    secondary: Vec<bool>,
}

/// Typos a word may contain: one in four, only for words of 5+ characters.
fn max_typos(word: &str) -> u16 {
    let len = word.chars().count();
    if len >= 5 { (len / 4) as u16 } else { 0 }
}

/// Items matching `query` as `(index into items, score)`, best first; equal
/// scores keep the list order. `fields` returns the item's searchable fields,
/// all of the same standing (see [`rank_tiered`] for fields that count less).
/// Each word must match in at least one field (different words may match in
/// different fields); an item's score is the sum of its words' best field
/// scores. An empty query matches every item with score 0, in list order.
pub fn rank<T>(query: &str, items: &[T], fields: impl Fn(&T) -> Vec<String>) -> Vec<(usize, u32)> {
    rank_tiered(query, items, fields, |_| Vec::new())
}

/// Like [`rank`], with two tiers of fields: the item's own name (`primary`)
/// and what only describes or locates it (`secondary`: a provider, a path, a
/// description, keywords). A word still matches in either tier, but a match
/// in a secondary field counts for half. Without this, every model of a
/// provider named `openrouter` outranks the model `claude-opus` for the query
/// `op`, on the provider's name alone.
pub fn rank_tiered<T>(
    query: &str,
    items: &[T],
    primary: impl Fn(&T) -> Vec<String>,
    secondary: impl Fn(&T) -> Vec<String>,
) -> Vec<(usize, u32)> {
    let query = normalize(query);
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return (0..items.len()).map(|i| (i, 0)).collect();
    }
    // Per item: its fields, primary first, each flagged secondary or not.
    let fields: Vec<Vec<(String, bool)>> = items
        .iter()
        .map(|item| {
            let own = primary(item).into_iter().map(|f| (normalize(&f), false));
            let rest = secondary(item).into_iter().map(|f| (normalize(&f), true));
            own.chain(rest).collect()
        })
        .collect();
    let width = fields.iter().map(Vec::len).max().unwrap_or(0);
    // Per field position: the items having it, as haystacks, and whether
    // each is a secondary field.
    let columns: Vec<Column> = (0..width)
        .map(|c| {
            let mut column = Column::default();
            for (i, f) in fields.iter().enumerate() {
                if let Some((text, secondary)) = f.get(c) {
                    column.owners.push(i);
                    column.haystacks.push(text.as_str());
                    column.secondary.push(*secondary);
                }
            }
            column
        })
        .collect();

    let mut totals: Vec<Option<u32>> = vec![Some(0); items.len()];
    for word in words {
        let pattern = Pattern::new(word, PatternConfig::default()).max_typos(Some(max_typos(word)));
        let mut best: Vec<Option<u32>> = vec![None; items.len()];
        let mut matcher = Matcher::from_patterns(&[pattern], &Config::default());
        for column in &columns {
            for m in matcher.match_list(&column.haystacks) {
                let at = m.index as usize;
                let score = if column.secondary[at] {
                    m.score as u32 / 2
                } else {
                    m.score as u32
                };
                let slot = &mut best[column.owners[at]];
                *slot = Some(slot.map_or(score, |s| s.max(score)));
            }
        }
        for (total, best) in totals.iter_mut().zip(best) {
            *total = total.zip(best).map(|(t, b)| t + b);
        }
    }
    let mut hits: Vec<(usize, u32)> = totals
        .into_iter()
        .enumerate()
        .filter_map(|(i, t)| Some((i, t?)))
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    hits
}

#[cfg(test)]
mod tests {
    use super::{rank, rank_tiered};

    fn names(query: &str, items: &[&str]) -> Vec<String> {
        rank(query, items, |s| vec![s.to_string()])
            .into_iter()
            .map(|(i, _)| items[i].to_string())
            .collect()
    }

    #[test]
    fn empty_query_matches_everything_in_order() {
        let items = ["b", "a", "c"];
        assert_eq!(
            rank("", &items, |s| vec![s.to_string()]),
            [(0, 0), (1, 0), (2, 0)]
        );
        assert_eq!(names("  ", &items), ["b", "a", "c"]);
    }

    #[test]
    fn every_word_must_match() {
        // `from_patterns` requires all patterns (AND), as the design assumes.
        let items = ["coder delegate", "coder", "delegate"];
        assert_eq!(names("coder delegate", &items), ["coder delegate"]);
        assert!(names("coder zzz", &items).is_empty());
        assert_eq!(names("cdl", &["Coder Delegate"]), ["Coder Delegate"]);
    }

    #[test]
    fn words_match_across_fields() {
        let items = [("thor", "peer-9f3e"), ("odin", "peer-1111")];
        let hits = rank("thor 9f3e", &items, |(a, b)| {
            vec![a.to_string(), b.to_string()]
        });
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), [0]);
        assert!(
            rank("coder zzz", &items, |(a, b)| vec![
                a.to_string(),
                b.to_string()
            ])
            .is_empty()
        );
    }

    #[test]
    fn a_word_never_spans_two_fields() {
        // `cdr` is a subsequence of the joined text but of no single field.
        let items = [("Coder Delegate", "cdr"), ("def-qmcp", "QueryMT MCP tools")];
        let hits = rank("cdr", &items, |(a, b)| vec![a.to_string(), b.to_string()]);
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn separators_do_not_block_a_match() {
        assert_eq!(
            names("sonnet 4.5", &["claude-sonnet-4-5"]),
            ["claude-sonnet-4-5"]
        );
        assert_eq!(names("gpt5", &["gpt-5", "other"]), ["gpt-5"]);
        assert_eq!(names("claude_sonnet", &["claude-sonnet-4-5"]).len(), 1);
        assert_eq!(names("a/b", &["a:b"]).len(), 1);
    }

    #[test]
    fn typos_only_for_long_words() {
        assert_eq!(names("sonet", &["Sonnet", "Haiku"]), ["Sonnet"]);
        assert!(names("gpz", &["gpt"]).is_empty());
    }

    #[test]
    fn contiguous_ranks_first() {
        let items = ["Default (Speech)", "def-qmcp"];
        assert_eq!(names("qmcp", &items), ["def-qmcp"]);
        let items = ["d-e-f", "def-x"];
        assert_eq!(names("def", &items)[0], "def-x");
        let items = ["xx claude-sonnet", "sonnet-4"];
        assert_eq!(names("sonnet", &items)[0], "sonnet-4");
    }

    #[test]
    fn ties_keep_list_order() {
        assert_eq!(names("thor", &["thor-a", "thor-b"]), ["thor-a", "thor-b"]);
        assert_eq!(names("thor", &["thor-b", "thor-a"]), ["thor-b", "thor-a"]);
    }

    #[test]
    fn operators_are_literal() {
        // `^foo` is not "starts with foo": the caret is an ordinary character
        // (the matcher sees the pattern text, so only a literal `^` matches).
        let items = ["foo", "a^foo", "barfoo"];
        let hits = names("^foo", &items);
        assert!(hits.contains(&"a^foo".to_string()));
        assert!(!hits.contains(&"foo".to_string()));
        // A negated pattern would exclude `foo!foo`; a literal one keeps it.
        assert!(names("!foo", &["foo!foo"]).contains(&"foo!foo".to_string()));
        assert!(names("!foo", &["foo"]).is_empty());
    }

    /// `(name, provider)` items, ranked with the provider as a secondary field.
    fn by_name(query: &str, items: &[(&str, &str)]) -> Vec<String> {
        rank_tiered(
            query,
            items,
            |(name, _)| vec![name.to_string()],
            |(_, provider)| vec![provider.to_string()],
        )
        .into_iter()
        .map(|(i, _)| items[i].0.to_string())
        .collect()
    }

    #[test]
    fn a_name_match_beats_a_match_on_the_provider_alone() {
        // `op` begins the provider `openrouter` (a prefix match), but the
        // second model's own name has it at a word start.
        let items = [
            ("inkling-2-mini", "openrouter"),
            ("mistral-large", "openrouter"),
            ("claude-opus-4", "anthropic"),
        ];
        assert_eq!(by_name("op", &items)[0], "claude-opus-4");
        // The same with every field of the same standing: the provider wins.
        let flat = rank("op", &items, |(n, p)| vec![n.to_string(), p.to_string()]);
        assert_ne!(items[flat[0].0].0, "claude-opus-4");
    }

    #[test]
    fn a_secondary_field_alone_still_matches() {
        let items = [
            ("inkling-2-mini", "openrouter"),
            ("claude-opus-4", "anthropic"),
        ];
        assert_eq!(by_name("openrouter", &items), ["inkling-2-mini"]);
        // Words may split across tiers: a name word and a provider word.
        assert_eq!(by_name("opus anthropic", &items), ["claude-opus-4"]);
    }

    #[test]
    fn within_a_provider_the_better_name_leads() {
        let items = [
            ("inkling-2-mini", "openrouter"),
            ("claude-opus-4", "openrouter"),
        ];
        assert_eq!(by_name("op", &items)[0], "claude-opus-4");
    }

    #[test]
    fn model_search_cases() {
        let models = [
            "Claude Sonnet 4.5 claude-sonnet-4-5 anthropic",
            "GPT-5 gpt-5 openai",
            "Haiku claude-haiku-4-5 anthropic",
            "Gemini 2.5 gemini-2.5-pro google",
        ];
        let find = |q: &str| names(q, &models);
        assert_eq!(find("anthropic sonnet")[0], models[0]);
        assert_eq!(find("sonnet 4.5")[0], models[0]);
        assert_eq!(find("gpt5")[0], models[1]);
        assert_eq!(find("cl son")[0], models[0]);
        assert_eq!(find("sonet")[0], models[0]);
    }
}
