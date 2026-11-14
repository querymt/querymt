//! Standalone syntect highlighting, shared by tool output and Markdown fences.

use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, LazyLock},
};

use gpui_kit::base::{TextViewDefaults, input::HighlightStyleResolver, text::CodeBlock};
use gpui_kit::component::{Theme, highlighter::HighlightTheme};
use gpui_kit::{App, HighlightStyle, SharedString};
use syntect::{
    parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet},
    util::LinesWithEndings,
};

// Decode the grammar bundle once, not once per visible block or theme change.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);
type Highlights = Vec<(Range<usize>, &'static str)>;

/// Preserve the themed text defaults while sharing this grammar cache with Markdown.
/// Theme updates reinstall component defaults, so call this after applying the theme.
pub(crate) fn install(cx: &mut App) {
    let theme = Theme::global(cx).highlight_theme.clone();
    TextViewDefaults::global(cx)
        .with_code_block_highlighter(code_block_highlighter(theme))
        .install(cx);
}

fn code_block_highlighter(
    theme: Arc<HighlightTheme>,
) -> impl Fn(&CodeBlock) -> Vec<(Range<usize>, HighlightStyle)> + Send + Sync {
    move |block| match block.lang() {
        Some(lang) => highlight_code(lang.as_ref(), block.code().as_ref(), theme.as_ref()),
        None => Vec::new(),
    }
}

/// Gap-free byte-range styles, or no styles when the language is unknown.
pub(crate) fn highlight_code(
    language: &str,
    code: &str,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let Some(syntax) = find_syntax(language) else {
        return Vec::new();
    };
    resolve_styles(&parse(syntax, code), &(0..code.len()), resolver)
}

fn language_name(name: &str) -> SharedString {
    let name = name.to_lowercase();
    match name.as_str() {
        "plain" | "plaintext" => "text".into(),
        "jsonc" => "json".into(),
        "sh" => "bash".into(),
        "c++" => "cpp".into(),
        "cs" => "csharp".into(),
        "scss" => "css".into(),
        "ex" => "elixir".into(),
        "js" => "javascript".into(),
        "kt" | "kts" | "ktm" => "kotlin".into(),
        "makefile" => "make".into(),
        "md" | "mdx" => "markdown".into(),
        "markdown-inline" => "markdown_inline".into(),
        "php3" | "php4" | "php5" | "phtml" => "php".into(),
        "protobuf" => "proto".into(),
        "py" | "pyi" => "python".into(),
        "rb" => "ruby".into(),
        "rs" => "rust".into(),
        "ts" => "typescript".into(),
        "yml" => "yaml".into(),
        _ => name.into(),
    }
}

fn find_syntax(language: &str) -> Option<&'static SyntaxReference> {
    let syntaxes = &*SYNTAXES;
    let canonical = language_name(language);
    let alias = match canonical.as_ref() {
        "text" => return None,
        "csharp" => "cs",
        "make" => "Makefile",
        "markdown_inline" => "md",
        "embedded_template" | "ejs" => "erb",
        other => other,
    };
    [language, canonical.as_ref(), alias]
        .into_iter()
        .find_map(|token| syntaxes.find_syntax_by_token(token))
        .filter(|syntax| syntax.name != "Plain Text")
}

fn parse(syntax: &SyntaxReference, text: &str) -> Highlights {
    let syntaxes = &*SYNTAXES;
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut names = HashMap::<Scope, Option<&'static str>>::new();
    let mut highlights = Highlights::new();
    let mut offset = 0;

    let mut push = |range: Range<usize>, scopes: &ScopeStack, highlights: &mut Highlights| {
        if range.is_empty() {
            return;
        }
        // Delimiters inherit their container: string quotes should not become
        // punctuation-colored just because that is the innermost scope.
        let name = scopes
            .scopes
            .iter()
            .rev()
            .find_map(|scope| *names.entry(*scope).or_insert_with(|| semantic_name(*scope)));
        if let Some(name) = name {
            match highlights.last_mut() {
                Some((last, last_name)) if last.end == range.start && *last_name == name => {
                    last.end = range.end;
                }
                _ => highlights.push((range, name)),
            }
        }
    };

    for line in LinesWithEndings::from(text) {
        if let Ok(operations) = parser.parse_line(line, syntaxes) {
            let mut cursor = 0;
            for (index, operation) in operations {
                push(offset + cursor..offset + index, &scopes, &mut highlights);
                let _ = scopes.apply(&operation);
                cursor = index;
            }
            push(
                offset + cursor..offset + line.len(),
                &scopes,
                &mut highlights,
            );
        }
        offset += line.len();
    }
    highlights
}

fn semantic_name(scope: Scope) -> Option<&'static str> {
    let scope = scope.build_string();
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    };
    let name = if is("punctuation.definition.string")
        || is("punctuation.definition.comment")
        || is("invalid")
        || is("meta")
        || is("source")
        || is("text")
    {
        return None;
    } else if is("comment.block.documentation") || is("comment.line.documentation") {
        "comment.doc"
    } else if is("comment") {
        "comment"
    } else if is("constant.character.escape") {
        "string.escape"
    } else if is("string.regexp") {
        "string.regex"
    } else if is("string") {
        "string"
    } else if is("constant.numeric") {
        "number"
    } else if is("constant.language.boolean") {
        "boolean"
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        "constant"
    } else if is("keyword.operator") {
        "operator"
    } else if is("keyword.control.import") || is("meta.preprocessor") {
        "preproc"
    } else if is("keyword") || is("storage.modifier") {
        "keyword"
    } else if is("storage.type") {
        if [
            "class",
            "enum",
            "function",
            "impl",
            "interface",
            "module",
            "namespace",
            "struct",
            "trait",
            "type",
            "let",
            "var",
            "const",
        ]
        .iter()
        .any(|word| scope.contains(word))
        {
            "keyword"
        } else {
            "type"
        }
    } else if is("entity.name.function")
        || is("support.function")
        || is("variable.function")
        || is("meta.function-call")
    {
        "function"
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
    {
        "type"
    } else if is("entity.name.tag") {
        "tag"
    } else if is("entity.other.attribute-name") {
        "attribute"
    } else if is("entity.name.label") {
        "label"
    } else if is("entity.name.section") || is("markup.heading") {
        "title"
    } else if is("variable.language") {
        "variable.special"
    } else if is("variable.other.member")
        || is("variable.other.property")
        || is("support.variable.property")
        || is("meta.mapping.key")
    {
        "property"
    } else if is("variable") {
        "variable"
    } else if is("markup.bold") {
        "emphasis.strong"
    } else if is("markup.italic") {
        "emphasis"
    } else if is("markup.underline.link") {
        "link_uri"
    } else if is("markup.raw") {
        "text.literal"
    } else if is("markup.list") {
        "punctuation.list_marker"
    } else if is("punctuation.section") || is("punctuation.definition.block") {
        "punctuation.bracket"
    } else if is("punctuation.separator") || is("punctuation.terminator") {
        "punctuation.delimiter"
    } else if is("punctuation") {
        "punctuation"
    } else {
        return None;
    };
    Some(name)
}

fn resolve_styles(
    highlights: &[(Range<usize>, &'static str)],
    range: &Range<usize>,
    resolver: &dyn HighlightStyleResolver,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let first = highlights.partition_point(|(highlight, _)| highlight.end <= range.start);
    let mut runs = Vec::new();
    let mut cursor = range.start;
    for (highlight, name) in &highlights[first..] {
        if highlight.start >= range.end {
            break;
        }
        let start = highlight.start.max(range.start);
        let end = highlight.end.min(range.end);
        if start >= end || end <= cursor {
            continue;
        }
        if cursor < start {
            runs.push((cursor..start, HighlightStyle::default()));
        }
        runs.push((start..end, resolver.style(name).unwrap_or_default()));
        cursor = end;
    }
    if cursor < range.end {
        runs.push((cursor..range.end, HighlightStyle::default()));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::{code_block_highlighter, find_syntax, highlight_code, semantic_name};
    use gpui_kit::base::text::CodeBlock;
    use gpui_kit::component::highlighter::HighlightTheme;
    use syntect::parsing::Scope;

    #[test]
    fn supports_common_language_aliases() {
        for language in [
            "rust",
            "rs",
            "RS",
            "json",
            "jsonc",
            "js",
            "ts",
            "py",
            "pyi",
            "sh",
            "Makefile",
            "cs",
            "yml",
            "markdown-inline",
            "ejs",
        ] {
            assert!(find_syntax(language).is_some(), "{language}");
        }
    }

    #[test]
    fn unknown_languages_plain_text_and_empty_input_have_no_styles() {
        let theme = HighlightTheme::default_dark();
        for language in ["no-such-language", "plain", "plaintext", "text", ""] {
            assert!(highlight_code(language, "let x = 1;", theme.as_ref()).is_empty());
        }
        assert!(highlight_code("rs", "", theme.as_ref()).is_empty());
    }

    #[test]
    fn styles_cover_utf8_and_multiline_input_without_gaps() {
        for code in [
            "/* first\nsecond */\nfn main() { let s = \"\u{3bb}\"; }\n",
            "let s = \"\u{1f600}\";\r\n// comment\r\n",
            "let s = \"unterminated",
        ] {
            for theme in [
                HighlightTheme::default_dark(),
                HighlightTheme::default_light(),
            ] {
                let spans = highlight_code("rs", code, theme.as_ref());
                let mut end = 0;
                for (range, _) in spans {
                    assert_eq!(range.start, end);
                    assert!(range.end > range.start);
                    assert!(code.is_char_boundary(range.start) && code.is_char_boundary(range.end));
                    end = range.end;
                }
                assert_eq!(end, code.len());
            }
        }
    }

    #[test]
    fn semantic_scopes_match_theme_names_not_similar_prefixes() {
        for (scope, name) in [
            ("comment.block.rust", Some("comment")),
            ("comment.block.documentation.rust", Some("comment.doc")),
            ("constant.character.escape.rust", Some("string.escape")),
            ("constant.numeric.rust", Some("number")),
            ("entity.name.function.rust", Some("function")),
            ("punctuation.definition.string.begin.rust", None),
            ("comments.not-a-comment", None),
            ("source.rust", None),
        ] {
            assert_eq!(
                semantic_name(Scope::new(scope).expect("valid test scope")),
                name
            );
        }
    }

    #[test]
    fn multiline_comments_keep_their_style_on_subsequent_lines() {
        let code = "/* first\nsecond */\nfn main() {}";
        let theme = HighlightTheme::default_dark();
        let spans = highlight_code("rs", code, theme.as_ref());
        let comment = theme.style("comment").expect("theme defines comments");
        for word in ["first", "second"] {
            let offset = code.find(word).expect("test word is present");
            let (_, style) = spans
                .iter()
                .find(|(range, _)| range.contains(&offset))
                .expect("style covers word");
            assert_eq!(*style, comment);
        }
    }

    #[test]
    fn markdown_callback_matches_tool_highlighting_for_each_theme() {
        for theme in [
            HighlightTheme::default_dark(),
            HighlightTheme::default_light(),
        ] {
            let highlighter = code_block_highlighter(theme.clone());
            let code = "fn main() { let s = \"hi\"; }";
            let block = CodeBlock::from_code(code, Some("rs"));
            assert_eq!(
                highlighter(&block),
                highlight_code("rs", code, theme.as_ref())
            );
            let plain = CodeBlock::from_code(code, None::<&str>);
            assert!(highlighter(&plain).is_empty());
            let unknown = CodeBlock::from_code(code, Some("no-such-language"));
            assert!(highlighter(&unknown).is_empty());
        }
    }
}
