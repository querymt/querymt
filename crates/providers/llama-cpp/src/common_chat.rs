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
    /// Tool schemas the prompt was rendered with; the parser uses them to
    /// keep string parameters verbatim.
    pub tools: Vec<querymt::chat::Tool>,
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
    /// Counter that splits output tokens into reasoning and answer tokens.
    pub(crate) fn reasoning_counter(&self) -> ReasoningTokenCounter {
        ReasoningTokenCounter::new(self.reasoning_format, self.starts_in_thinking)
    }

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

/// Counts how many generated tokens belong to reasoning blocks, so usage can
/// report them as `reasoning_tokens` instead of lumping them into
/// `output_tokens`. Tokens that open or close a block count as reasoning.
#[derive(Debug)]
pub(crate) struct ReasoningTokenCounter {
    in_thinking: bool,
    tail: String,
    reasoning: u32,
    reasoning_format: ReasoningFormat,
}

impl ReasoningTokenCounter {
    const TAIL_BYTES: usize = 64;

    /// Create a counter, optionally starting inside a reasoning block.
    pub(crate) fn new(reasoning_format: ReasoningFormat, starts_in_thinking: bool) -> Self {
        Self {
            in_thinking: starts_in_thinking,
            tail: String::new(),
            reasoning: 0,
            reasoning_format,
        }
    }

    /// Observe the text of one generated token.
    pub(crate) fn observe(&mut self, piece: &str) {
        self.tail.push_str(piece);
        if self.tail.len() > Self::TAIL_BYTES {
            let mut cut = self.tail.len() - Self::TAIL_BYTES;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.tail.drain(..cut);
        }
        let marker = if self.in_thinking {
            self.reasoning_format.close_tag()
        } else {
            self.reasoning_format.open_tag()
        };
        let counts = self.in_thinking;
        if self.tail.contains(marker) {
            self.in_thinking = !self.in_thinking;
            self.tail.clear();
            // The marker token itself belongs to the reasoning block.
            self.reasoning += 1;
        } else if counts {
            self.reasoning += 1;
        }
    }

    /// Text injected by the thinking budget (always closes the block);
    /// `tokens` is how many tokens it took.
    pub(crate) fn observe_forced(&mut self, text: &str, tokens: u32) {
        self.reasoning = self.reasoning.saturating_add(tokens);
        if text.contains(self.reasoning_format.close_tag()) {
            self.in_thinking = false;
            self.tail.clear();
        }
    }

    /// Split `output_tokens` (which includes reasoning) into
    /// `(output_tokens, reasoning_tokens)` with no overlap.
    pub(crate) fn split(&self, output_tokens: u32) -> (u32, u32) {
        let reasoning = self.reasoning.min(output_tokens);
        (output_tokens - reasoning, reasoning)
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

    /// Text inserted to close a reasoning block in `reasoning_format`.
    pub(crate) fn close_marker(reasoning_format: ReasoningFormat) -> &'static str {
        match reasoning_format {
            ReasoningFormat::ThinkTags => "\n</think>\n\n",
            ReasoningFormat::GemmaChannel { .. } => "<channel|>",
        }
    }

    /// Cap `budget` so that, once it is spent, the `marker_tokens` closing
    /// tokens plus at least one answer token still fit in `max_tokens`.
    /// Returns `None` when `max_tokens` is too small to ever fit them.
    pub(crate) fn cap_budget(budget: u32, max_tokens: u32, marker_tokens: u32) -> Option<u32> {
        let capacity = max_tokens.checked_sub(marker_tokens.saturating_add(1))?;
        (capacity > 0).then(|| budget.min(capacity))
    }

    /// Create a budget tracker with an explicit initial reasoning state.
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
        Some(Self::close_marker(self.reasoning_format))
    }
}

/// Number of leading bytes of `buffer` that can be emitted now. While the
/// stream is partial, a trailing prefix of `marker` is held back in case the
/// next chunk completes it.
fn safe_emit_len(buffer: &str, marker: &str, is_partial: bool) -> usize {
    if !is_partial {
        return buffer.len();
    }
    let keep = longest_suffix_prefix_len(buffer, marker);
    buffer.len().saturating_sub(keep)
}

/// Length of the longest proper prefix of `marker` that `buffer` ends with,
/// considering only char boundaries of `marker`.
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
mod reasoning_counter_tests {
    use super::*;

    /// Feed one token per piece and return the `(reasoning, output)` split.
    fn count(format: ReasoningFormat, starts: bool, pieces: &[&str]) -> (u32, u32) {
        let mut counter = ReasoningTokenCounter::new(format, starts);
        for piece in pieces {
            counter.observe(piece);
        }
        counter.split(pieces.len() as u32)
    }

    #[test]
    fn counts_open_block_started_by_prompt() {
        // 3 thinking tokens, the closing tag, then 2 answer tokens.
        let (out, reasoning) = count(
            ReasoningFormat::ThinkTags,
            true,
            &["a", "b", "c", "</think>", "x", "y"],
        );
        assert_eq!((out, reasoning), (2, 4));
    }

    #[test]
    fn counts_block_opened_by_model() {
        let (out, reasoning) = count(
            ReasoningFormat::ThinkTags,
            false,
            &["<think>", "a", "b", "</think>", "x"],
        );
        assert_eq!((out, reasoning), (1, 4));
    }

    #[test]
    fn handles_markers_split_across_tokens() {
        let (out, reasoning) = count(
            ReasoningFormat::ThinkTags,
            true,
            &["a", "</", "think", ">", "x"],
        );
        // `</`, `think` and `>` are part of the closing marker.
        assert_eq!((out, reasoning), (1, 4));
    }

    #[test]
    fn no_reasoning_means_everything_is_output() {
        assert_eq!(
            count(ReasoningFormat::ThinkTags, false, &["a", "b", "c"]),
            (3, 0)
        );
    }

    #[test]
    fn gemma_channel_markers() {
        let (out, reasoning) = count(
            ReasoningFormat::GemmaChannel {
                implicit_leading_reasoning_prefix: false,
            },
            false,
            &["<|channel>", "thought", "x", "<channel|>", "ok"],
        );
        assert_eq!((out, reasoning), (1, 4));
    }

    #[test]
    fn forced_close_counts_tokens_and_ends_block() {
        let mut counter = ReasoningTokenCounter::new(ReasoningFormat::ThinkTags, true);
        counter.observe("a");
        counter.observe_forced("\n</think>\n\n", 4);
        counter.observe("answer");
        assert_eq!(counter.split(6), (1, 5));
    }

    #[test]
    fn split_never_exceeds_output_tokens() {
        let mut counter = ReasoningTokenCounter::new(ReasoningFormat::ThinkTags, true);
        counter.observe_forced("</think>", 10);
        let (out, reasoning) = counter.split(3);
        assert_eq!(out + reasoning, 3);
    }
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
    fn thinking_budget_cap_reserves_marker_and_answer_token() {
        // Budget at or above max_tokens is reduced to leave room.
        assert_eq!(ThinkingBudget::cap_budget(100, 10, 3), Some(6));
        assert_eq!(ThinkingBudget::cap_budget(10, 10, 3), Some(6));
        // Budget just below max_tokens is also reduced.
        assert_eq!(ThinkingBudget::cap_budget(9, 10, 3), Some(6));
        // A budget that already fits is kept.
        assert_eq!(ThinkingBudget::cap_budget(4, 10, 3), Some(4));
        // No room for marker plus an answer token: no budget.
        assert_eq!(ThinkingBudget::cap_budget(4, 4, 3), None);
        assert_eq!(ThinkingBudget::cap_budget(4, 2, 3), None);
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
