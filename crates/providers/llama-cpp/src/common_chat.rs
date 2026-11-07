use crate::chat_format::ParsedDelta;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ReasoningFormat {
    ThinkTags,
    GemmaChannel {
        implicit_leading_reasoning_prefix: bool,
    },
}

impl ReasoningFormat {
    pub(crate) fn detect(prompt: &str) -> Self {
        if prompt.contains("<|channel>")
            || prompt.contains("<channel|>")
            || prompt.contains("<|think|>")
        {
            Self::GemmaChannel {
                implicit_leading_reasoning_prefix: prompt.contains("<|think|>"),
            }
        } else {
            Self::ThinkTags
        }
    }

    pub(crate) fn open_tag(self) -> &'static str {
        match self {
            Self::ThinkTags => "<think>",
            Self::GemmaChannel { .. } => "<|channel>",
        }
    }

    pub(crate) fn close_tag(self) -> &'static str {
        match self {
            Self::ThinkTags => "</think>",
            Self::GemmaChannel { .. } => "<channel|>",
        }
    }

    pub(crate) fn strip_reasoning_prefix(self, text: &str) -> String {
        match self {
            Self::GemmaChannel { .. } => text.strip_prefix("thought\n").unwrap_or(text).to_string(),
            Self::ThinkTags => text.to_string(),
        }
    }

    fn initial_state(self, starts_in_thinking: bool, pending: &mut String) -> ThinkingState {
        if starts_in_thinking {
            if let Self::GemmaChannel {
                implicit_leading_reasoning_prefix: true,
            } = self
            {
                pending.push_str("thought\n");
            }
            ThinkingState::Thinking
        } else {
            ThinkingState::Content
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChatTemplateResult {
    pub prompt: String,
    pub grammar: Option<ToolGrammar>,
    pub preserved_tokens: Vec<String>,
    pub additional_stops: Vec<String>,
    pub starts_in_thinking: bool,
    pub reasoning_format: ReasoningFormat,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolGrammar {
    pub grammar: String,
    pub root: &'static str,
    pub lazy: bool,
    pub triggers: Vec<GrammarTrigger>,
}

#[derive(Debug, Clone)]
pub(crate) struct GrammarTrigger {
    pub value: String,
}

impl ChatTemplateResult {
    pub(crate) fn streaming_state(&self) -> ChatStreamingState {
        ChatStreamingState::new(self.reasoning_format, self.starts_in_thinking)
    }
}

pub(crate) fn prompt_starts_in_thinking(prompt: &str, reasoning_format: ReasoningFormat) -> bool {
    let open_tag = reasoning_format.open_tag();
    let close_tag = reasoning_format.close_tag();
    let mut rest = prompt;
    let mut depth = 0usize;

    loop {
        let open = rest.find(open_tag);
        let close = rest.find(close_tag);
        match (open, close) {
            (Some(open_pos), Some(close_pos)) if open_pos < close_pos => {
                depth += 1;
                rest = &rest[open_pos + open_tag.len()..];
            }
            (_, Some(close_pos)) => {
                depth = depth.saturating_sub(1);
                rest = &rest[close_pos + close_tag.len()..];
            }
            (Some(open_pos), None) => {
                depth += 1;
                rest = &rest[open_pos + open_tag.len()..];
            }
            (None, None) => break,
        }
    }

    depth > 0
}

#[derive(Debug)]
pub(crate) struct ChatStreamingState {
    parser: ThinkingStreamParser,
}

impl ChatStreamingState {
    pub(crate) fn new(reasoning_format: ReasoningFormat, starts_in_thinking: bool) -> Self {
        Self {
            parser: ThinkingStreamParser::new(reasoning_format, starts_in_thinking),
        }
    }

    pub(crate) fn update(&mut self, text_added: &str, is_partial: bool) -> Vec<ParsedDelta> {
        self.parser.push(text_added, is_partial)
    }

    pub(crate) fn finish(&mut self) -> Vec<ParsedDelta> {
        self.parser.push("", false)
    }
}

#[derive(Debug)]
struct ThinkingStreamParser {
    state: ThinkingState,
    pending: String,
    reasoning_format: ReasoningFormat,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
enum ThinkingState {
    #[default]
    Content,
    Thinking,
}

impl ThinkingStreamParser {
    fn new(reasoning_format: ReasoningFormat, starts_in_thinking: bool) -> Self {
        let mut pending = String::new();
        let state = reasoning_format.initial_state(starts_in_thinking, &mut pending);
        Self {
            state,
            pending,
            reasoning_format,
        }
    }

    fn push(&mut self, text: &str, is_partial: bool) -> Vec<ParsedDelta> {
        self.pending.push_str(text);
        let mut deltas = Vec::new();
        let open_tag = self.reasoning_format.open_tag();
        let close_tag = self.reasoning_format.close_tag();

        loop {
            match self.state {
                ThinkingState::Content => {
                    if let Some(pos) = self.pending.find(open_tag) {
                        let content = self.pending[..pos].to_string();
                        if !content.is_empty() {
                            deltas.push(ParsedDelta::Content(content));
                        }
                        self.pending.drain(..pos + open_tag.len());
                        self.state = ThinkingState::Thinking;
                    } else {
                        let emit_len = safe_emit_len(&self.pending, open_tag, is_partial);
                        if emit_len == 0 {
                            break;
                        }
                        let content = self.pending[..emit_len].to_string();
                        self.pending.drain(..emit_len);
                        if !content.is_empty() {
                            deltas.push(ParsedDelta::Content(content));
                        }
                        break;
                    }
                }
                ThinkingState::Thinking => {
                    if let Some(pos) = self.pending.find(close_tag) {
                        let thinking = self
                            .reasoning_format
                            .strip_reasoning_prefix(&self.pending[..pos]);
                        if !thinking.trim().is_empty() {
                            deltas.push(ParsedDelta::Thinking(thinking));
                        }
                        self.pending.drain(..pos + close_tag.len());
                        self.state = ThinkingState::Content;
                    } else {
                        let emit_len = safe_emit_len(&self.pending, close_tag, is_partial);
                        if emit_len == 0 {
                            break;
                        }
                        let thinking = self
                            .reasoning_format
                            .strip_reasoning_prefix(&self.pending[..emit_len]);
                        self.pending.drain(..emit_len);
                        if !thinking.is_empty() {
                            deltas.push(ParsedDelta::Thinking(thinking));
                        }
                        break;
                    }
                }
            }
        }

        if !is_partial && !self.pending.is_empty() {
            let remaining = std::mem::take(&mut self.pending);
            match self.state {
                ThinkingState::Content => deltas.push(ParsedDelta::Content(remaining)),
                ThinkingState::Thinking => {
                    let thinking = self.reasoning_format.strip_reasoning_prefix(&remaining);
                    if !thinking.is_empty() {
                        deltas.push(ParsedDelta::Thinking(thinking));
                    }
                }
            }
        }

        deltas
    }
}

/// Enforces `thinking_budget`: counts tokens generated inside a reasoning
/// block and says when the block has to be closed.
#[derive(Debug)]
pub(crate) struct ThinkingBudget {
    budget: u32,
    used: u32,
    in_thinking: bool,
    tail: String,
    reasoning_format: ReasoningFormat,
}

impl ThinkingBudget {
    /// Bytes of recent output kept to find markers split across tokens.
    const TAIL_BYTES: usize = 64;

    /// `None` when no budget is configured.
    pub(crate) fn new(budget: Option<u32>, result: &ChatTemplateResult) -> Option<Self> {
        budget.map(|budget| {
            Self::with_state(budget, result.reasoning_format, result.starts_in_thinking)
        })
    }

    fn with_state(budget: u32, reasoning_format: ReasoningFormat, in_thinking: bool) -> Self {
        Self {
            budget,
            used: 0,
            in_thinking,
            tail: String::new(),
            reasoning_format,
        }
    }

    /// Observe the text of one generated token. Returns the text to insert
    /// when the budget is spent while the model is still reasoning.
    pub(crate) fn observe(&mut self, piece: &str) -> Option<&'static str> {
        self.tail.push_str(piece);
        if self.tail.len() > Self::TAIL_BYTES {
            let mut cut = self.tail.len() - Self::TAIL_BYTES;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.tail.drain(..cut);
        }
        if !self.in_thinking {
            if self.tail.contains(self.reasoning_format.open_tag()) {
                self.in_thinking = true;
                self.tail.clear();
            }
            return None;
        }
        if self.tail.contains(self.reasoning_format.close_tag()) {
            self.in_thinking = false;
            self.tail.clear();
            return None;
        }
        self.used = self.used.saturating_add(1);
        if self.used < self.budget {
            return None;
        }
        self.in_thinking = false;
        self.tail.clear();
        Some(match self.reasoning_format {
            ReasoningFormat::ThinkTags => "\n</think>\n\n",
            ReasoningFormat::GemmaChannel { .. } => "<channel|>",
        })
    }
}

fn safe_emit_len(buffer: &str, marker: &str, is_partial: bool) -> usize {
    if !is_partial {
        return buffer.len();
    }
    let keep = longest_suffix_prefix_len(buffer, marker);
    buffer.len().saturating_sub(keep)
}

fn longest_suffix_prefix_len(buffer: &str, marker: &str) -> usize {
    let max = buffer.len().min(marker.len().saturating_sub(1));
    marker
        .char_indices()
        .map(|(index, _)| index)
        .filter(|&len| len > 0 && len <= max)
        .rev()
        .find(|&len| buffer.ends_with(&marker[..len]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(
        chunks: &[(&str, bool)],
        reasoning_format: ReasoningFormat,
        starts_in_thinking: bool,
    ) -> Vec<ParsedDelta> {
        let mut state = ChatStreamingState::new(reasoning_format, starts_in_thinking);
        let mut deltas = Vec::new();
        for (chunk, partial) in chunks {
            deltas.extend(state.update(chunk, *partial));
        }
        deltas.extend(state.finish());
        deltas
    }

    fn observe_all(budget: &mut ThinkingBudget, pieces: &[&str]) -> Vec<Option<&'static str>> {
        pieces.iter().map(|piece| budget.observe(piece)).collect()
    }

    #[test]
    fn thinking_budget_closes_reasoning_once_spent() {
        let mut budget = ThinkingBudget::with_state(3, ReasoningFormat::ThinkTags, true);
        assert_eq!(
            observe_all(&mut budget, &["a", "b", "c", "answer"]),
            [None, None, Some("\n</think>\n\n"), None],
            "closed exactly once, at the budget"
        );
    }

    #[test]
    fn thinking_budget_ignores_reasoning_closed_in_time_and_counts_reopened_blocks() {
        let mut budget = ThinkingBudget::with_state(3, ReasoningFormat::ThinkTags, false);
        // Not reasoning yet; a split open tag starts the block.
        assert_eq!(observe_all(&mut budget, &["hi", "<th", "ink>"]), [None; 3]);
        // Closed before the budget: nothing is forced.
        assert_eq!(observe_all(&mut budget, &["x", "</think>", "y"]), [None; 3]);
        // A second block shares the per-reply budget (1 token used so far).
        assert_eq!(
            observe_all(&mut budget, &["<think>", "p", "q"]),
            [None, None, Some("\n</think>\n\n")]
        );
    }

    #[test]
    fn finds_partial_multibyte_marker_without_invalid_slices() {
        let marker = "─end";
        assert_eq!(longest_suffix_prefix_len("text─", marker), "─".len());
        assert_eq!(longest_suffix_prefix_len("text─en", marker), "─en".len());
        assert_eq!(longest_suffix_prefix_len("text", marker), 0);
    }

    #[test]
    fn detects_unclosed_prompt_thinking_block() {
        assert!(prompt_starts_in_thinking(
            "user<think>",
            ReasoningFormat::ThinkTags
        ));
        assert!(!prompt_starts_in_thinking(
            "<think>x</think>done",
            ReasoningFormat::ThinkTags,
        ));
    }

    #[test]
    fn swallows_empty_thinking_block() {
        let deltas = collect(
            &[("<think>\n\n</think>\n\nHello", true)],
            ReasoningFormat::ThinkTags,
            false,
        );
        assert_eq!(deltas, vec![ParsedDelta::Content("\n\nHello".to_string())]);
    }

    #[test]
    fn handles_split_thinking_markers() {
        let deltas = collect(
            &[("<thi", true), ("nk>abc</thi", true), ("nk>Hi", true)],
            ReasoningFormat::ThinkTags,
            false,
        );
        assert_eq!(
            deltas,
            vec![
                ParsedDelta::Thinking("abc".to_string()),
                ParsedDelta::Content("Hi".to_string())
            ]
        );
    }

    #[test]
    fn flushes_unclosed_thinking_as_thinking() {
        let deltas = collect(&[("<think>abc", true)], ReasoningFormat::ThinkTags, false);
        assert_eq!(deltas, vec![ParsedDelta::Thinking("abc".to_string())]);
    }

    #[test]
    fn starts_stream_in_thinking_when_prompt_left_open() {
        let deltas = collect(
            &[("thinking...</think>Hello", true)],
            ReasoningFormat::ThinkTags,
            true,
        );
        assert_eq!(
            deltas,
            vec![
                ParsedDelta::Thinking("thinking...".to_string()),
                ParsedDelta::Content("Hello".to_string())
            ]
        );
    }

    #[test]
    fn parses_gemma_channel_reasoning_blocks() {
        let deltas = collect(
            &[("<|channel>thought\nplan<channel|>Done", true)],
            ReasoningFormat::GemmaChannel {
                implicit_leading_reasoning_prefix: false,
            },
            false,
        );
        assert_eq!(
            deltas,
            vec![
                ParsedDelta::Thinking("plan".to_string()),
                ParsedDelta::Content("Done".to_string())
            ]
        );
    }

    #[test]
    fn parses_implicit_gemma_channel_reasoning_prefix() {
        let deltas = collect(
            &[("draft answer<channel|>Final", true)],
            ReasoningFormat::GemmaChannel {
                implicit_leading_reasoning_prefix: true,
            },
            true,
        );
        assert_eq!(
            deltas,
            vec![
                ParsedDelta::Thinking("draft answer".to_string()),
                ParsedDelta::Content("Final".to_string())
            ]
        );
    }
}
