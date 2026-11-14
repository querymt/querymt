//! Syntax-highlighted code for tool output (reads, symbol lookups, diffs).
//!
//! Highlighting uses the app's syntect adapter, colored by the component
//! `HighlightTheme`, so it follows dark/light mode.
//! Results are cached by (language, text, theme): transcript rows re-render
//! every frame while visible, and syntect is too slow to rerun each time.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::highlighter::HighlightTheme;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::kit;
use crate::highlight::highlight_code;
use crate::theme::Palette;

type Spans = Rc<Vec<(Range<usize>, HighlightStyle)>>;

const CACHE_LIMIT: usize = 256;

thread_local! {
    static CACHE: RefCell<HashMap<u64, Spans>> = RefCell::new(HashMap::new());
}

/// Highlighter input language for a file path: its extension (syntect matches
/// extensions and names), or the file name for extensionless files (`Makefile`).
pub fn language_for_path(path: &str) -> Option<String> {
    let name = path.trim().rsplit(['/', '\\']).next()?;
    if name.is_empty() {
        return None;
    }
    Some(match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext.to_lowercase(),
        _ => name.to_string(),
    })
}

/// Byte-range styles for `code`, cached; empty when the language is unknown.
fn spans(lang: &str, code: &str, theme: &Arc<HighlightTheme>) -> Spans {
    let mut h = DefaultHasher::new();
    lang.hash(&mut h);
    code.hash(&mut h);
    (Arc::as_ptr(theme) as usize).hash(&mut h);
    let key = h.finish();
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let styles: Spans = Rc::new(highlight_code(lang, code, theme.as_ref()));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_LIMIT {
            c.clear();
        }
        c.insert(key, styles.clone());
    });
    styles
}

/// Text split into lines with the highlight runs of each line, relative to it.
pub struct HighlightedLines {
    text: String,
    lines: Vec<Range<usize>>,
    spans: Spans,
}

impl HighlightedLines {
    pub fn new(text: &str, lang: Option<&str>, theme: &Arc<HighlightTheme>) -> Self {
        let text = text.to_string();
        let mut lines = Vec::new();
        let mut start = 0;
        for (ix, _) in text.match_indices('\n') {
            lines.push(start..ix);
            start = ix + 1;
        }
        if start < text.len() {
            lines.push(start..text.len());
        }
        let spans = match lang {
            Some(lang) if !text.is_empty() => spans(lang, &text, theme),
            _ => Rc::default(),
        };
        Self { text, lines, spans }
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Line `ix` (tabs expanded) as styled text.
    pub fn line(&self, ix: usize) -> AnyElement {
        let Some(range) = self.lines.get(ix).cloned() else { return div().into_any_element() };
        let raw = &self.text[range.clone()];
        if raw.is_empty() {
            return div().child(" ").into_any_element();
        }
        let first = self.spans.partition_point(|(r, _)| r.end <= range.start);
        let mut runs: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for (r, style) in &self.spans[first..] {
            if r.start >= range.end {
                break;
            }
            let (s, e) = (r.start.max(range.start) - range.start, r.end.min(range.end) - range.start);
            if s < e && style.color.is_some() {
                runs.push((s..e, *style));
            }
        }
        if !raw.contains('\t') {
            return StyledText::new(raw.to_string()).with_highlights(runs).into_any_element();
        }
        // Expanding tabs shifts byte offsets: rebuild runs against the new text.
        let mut out = String::with_capacity(raw.len() + 16);
        let mut map = Vec::with_capacity(raw.len() + 1);
        for (b, ch) in raw.char_indices() {
            while map.len() < b {
                map.push(out.len());
            }
            map.push(out.len());
            if ch == '\t' { out.push_str("    ") } else { out.push(ch) }
        }
        while map.len() <= raw.len() {
            map.push(out.len());
        }
        let runs = runs.into_iter().map(|(r, s)| (map[r.start]..map[r.end], s)).filter(|(r, _)| !r.is_empty());
        StyledText::new(out).with_highlights(runs).into_any_element()
    }
}

/// One piece of a read result: tool chatter (headers, notes) or numbered code.
enum Segment {
    Meta(String),
    Code { numbers: Vec<Option<u64>>, text: String },
}

/// `00197| code` → (197, "code").
fn numbered(line: &str) -> Option<(u64, &str)> {
    let (num, rest) = line.split_once('|')?;
    let digits = num.trim_start();
    if digits.len() < 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digits.parse().ok()?, rest.strip_prefix(' ').unwrap_or(rest)))
}

/// Split read/get_function/get_symbol output into headers and numbered code,
/// dropping the `<path>` / `<type>` / `<content>` wrappers of `read_tool`.
fn segments(output: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut any_numbered = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if matches!(trimmed, "<content>" | "</content>") || trimmed.starts_with("<type>") || trimmed.starts_with("<path>") {
            continue;
        }
        if let Some((n, code)) = numbered(line) {
            any_numbered = true;
            match out.last_mut() {
                Some(Segment::Code { numbers, text }) => {
                    numbers.push(Some(n));
                    text.push('\n');
                    text.push_str(code);
                }
                _ => out.push(Segment::Code { numbers: vec![Some(n)], text: code.to_string() }),
            }
        } else if !trimmed.is_empty() {
            out.push(Segment::Meta(trimmed.to_string()));
        }
    }
    if !any_numbered {
        // Unnumbered content (e.g. a raw file dump): the whole thing is code.
        let text = output.trim_end().to_string();
        let numbers = vec![None; text.lines().count()];
        return vec![Segment::Code { numbers, text }];
    }
    out
}

/// Highlighted, line-numbered code card (`clamp` caps the total code lines).
#[allow(clippy::too_many_arguments)]
fn code_card(id: impl Into<ElementId>, numbers: &[Option<u64>], text: &str, lang: Option<&str>, clamp: usize, flat: bool, theme: &Arc<HighlightTheme>, p: &Palette) -> (AnyElement, usize) {
    let lines = HighlightedLines::new(text, lang, theme);
    let shown = lines.len().min(clamp);
    let numbered = numbers.iter().any(Option::is_some);
    let width = numbers.iter().flatten().max().map_or(1, |n| n.to_string().len()).max(2);
    let gutter = px(width as f32 * 7.5 + 16.);
    let rows = (0..shown).map(|ix| {
        h_flex()
            .w_full()
            .items_start()
            .when(numbered, |el| {
                el.child(
                    div()
                        .flex_none()
                        .w(gutter)
                        .pr_3()
                        .text_right()
                        .text_color(p.dim.opacity(0.7))
                        .child(numbers.get(ix).copied().flatten().map(|n| n.to_string()).unwrap_or_default()),
                )
            })
            .child(div().flex_1().min_w_0().pr_3().text_color(p.text).child(lines.line(ix)))
            .into_any_element()
    });
    let card = div()
        .id(id)
        .w_full()
        .bg(p.bg)
        // Flat: sits inside a tool card that already draws the frame.
        .when(!flat, |el| el.rounded(px(8.)).border_1().border_color(p.border))
        // Long lines wrap; no horizontal scrolling.
        .overflow_hidden()
        .child(
            v_flex()
                .w_full()
                .when(!flat, |el| el.py_2())
                // Flat: fixed rows so the attached card's scroll height maps to lines.
                .when(flat, |el| el.py_1p5().line_height(px(18.)))
                .when(!numbered, |el| el.pl_3())
                .font_family(kit::MONO)
                .text_xs()
                .children(rows),
        )
        .into_any_element();
    (card, shown)
}

/// Read-tool result: headers as dim labels, code as highlighted cards.
/// `flat` renders only the code, borderless, to fill a card attached to the
/// tool header (the tool's own notes like "(File has more lines…)" are dropped).
pub fn read_output(id: &str, output: &str, path: Option<&str>, max_lines: usize, flat: bool, theme: &Arc<HighlightTheme>, p: &Palette) -> Div {
    let segments = segments(output);
    // A header naming a file sets the language of the code that follows it.
    let mut lang = path.and_then(language_for_path);
    let total: usize = segments.iter().map(|s| if let Segment::Code { numbers, .. } = s { numbers.len() } else { 0 }).sum();
    let mut budget = max_lines;
    let mut body = v_flex().w_full().when(!flat, |el| el.gap_1p5());
    let pad = |el: Div| if flat { el.px_3().py_1() } else { el.px_1() };
    let mut code_seen = false;
    for (ix, segment) in segments.iter().enumerate() {
        match segment {
            Segment::Meta(text) => {
                if (text.starts_with('/') || (!text.contains(' ') && text.contains('.')))
                    && let Some(l) = language_for_path(text)
                {
                    lang = Some(l);
                }
                if !flat {
                    body = body.child(pad(div()).text_xs().font_family(kit::MONO).text_color(p.dim).truncate().child(text.clone()));
                }
            }
            Segment::Code { numbers, text } if budget > 0 => {
                let (card, shown) = code_card(SharedString::from(format!("{id}-{ix}")), numbers, text, lang.as_deref(), budget, flat, theme, p);
                budget -= shown;
                // Flat: separate the code of several files/ranges with a rule.
                let card = if flat && code_seen { div().w_full().border_t_1().border_color(p.border).child(card).into_any_element() } else { card };
                code_seen = true;
                body = body.child(card);
            }
            Segment::Code { .. } => {}
        }
    }
    if total > max_lines && !flat {
        body = body.child(pad(div()).text_xs().text_color(p.dim).child(format!("… {} more lines", total - max_lines)));
    }
    body
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that pulls in gpui's `test` attribute and shadows `#[test]`.
    use super::{Segment, language_for_path, segments};

    #[test]
    fn parses_read_tool_and_get_function_output() {
        let read = "<path>/a/b.rs</path>\n<type>file</type>\n<content>\n00010| fn x() {\n00011| }\n\n(File has more lines.)\n</content>";
        let segs = segments(read);
        assert!(matches!(&segs[0], Segment::Code { numbers, text } if numbers == &vec![Some(10), Some(11)] && text == "fn x() {\n}"));
        assert!(matches!(&segs[1], Segment::Meta(t) if t.starts_with("(File")));

        let getf = "/a/b.rs\n- f [1-2] hash=x\n00001| fn f() {}\n00002|";
        let segs = segments(getf);
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[2], Segment::Code { text, .. } if text == "fn f() {}\n"));
    }

    #[test]
    fn rust_gets_theme_colors() {
        use gpui_kit::component::highlighter::HighlightTheme;
        use crate::highlight::highlight_code;
        let code = "fn main() { let x = \"hi\"; }";
        for theme in [HighlightTheme::default_dark(), HighlightTheme::default_light()] {
            let spans = highlight_code("rs", code, theme.as_ref());
            let colored: Vec<&str> = spans.iter().filter(|(_, s)| s.color.is_some()).map(|(r, _)| &code[r.clone()]).collect();
            assert!(colored.contains(&"fn"), "{colored:?}");
            assert!(colored.iter().any(|t| t.contains("hi")), "{colored:?}");
        }
    }

    #[test]
    fn languages_from_paths() {
        assert_eq!(language_for_path("/x/src/main.RS").as_deref(), Some("rs"));
        assert_eq!(language_for_path("Makefile").as_deref(), Some("Makefile"));
        assert_eq!(language_for_path(".bashrc").as_deref(), Some(".bashrc"));
    }
}
