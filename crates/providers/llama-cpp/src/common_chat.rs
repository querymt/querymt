use crate::chat_format::ParsedDelta;

#[derive(Debug, Clone)]
pub(crate) struct ChatTemplateResult {
    pub prompt: String,
    pub grammar: Option<ToolGrammar>,
    pub preserved_tokens: Vec<String>,
    pub additional_stops: Vec<String>,
    pub starts_in_thinking: bool,
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
        ChatStreamingState::new(self.starts_in_thinking)
    }
}

pub(crate) fn prompt_starts_in_thinking(prompt: &str) -> bool {
    let mut rest = prompt;
    let mut depth = 0usize;

    loop {
        let open = rest.find("<think>");
        let close = rest.find("</think>");
        match (open, close) {
            (Some(open_pos), Some(close_pos)) if open_pos < close_pos => {
                depth += 1;
                rest = &rest[open_pos + "<think>".len()..];
            }
            (_, Some(close_pos)) => {
                depth = depth.saturating_sub(1);
                rest = &rest[close_pos + "</think>".len()..];
            }
            (Some(open_pos), None) => {
                depth += 1;
                rest = &rest[open_pos + "<think>".len()..];
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
    pub(crate) fn new(starts_in_thinking: bool) -> Self {
        Self {
            parser: ThinkingStreamParser::new(starts_in_thinking),
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
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
enum ThinkingState {
    #[default]
    Content,
    Thinking,
}

impl ThinkingStreamParser {
    fn new(starts_in_thinking: bool) -> Self {
        Self {
            state: if starts_in_thinking {
                ThinkingState::Thinking
            } else {
                ThinkingState::Content
            },
            pending: String::new(),
        }
    }

    fn push(&mut self, text: &str, is_partial: bool) -> Vec<ParsedDelta> {
        self.pending.push_str(text);
        let mut deltas = Vec::new();

        loop {
            match self.state {
                ThinkingState::Content => {
                    if let Some(pos) = self.pending.find("<think>") {
                        let content = self.pending[..pos].to_string();
                        if !content.is_empty() {
                            deltas.push(ParsedDelta::Content(content));
                        }
                        self.pending.drain(..pos + "<think>".len());
                        self.state = ThinkingState::Thinking;
                    } else {
                        let emit_len = safe_emit_len(&self.pending, "<think>", is_partial);
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
                    if let Some(pos) = self.pending.find("</think>") {
                        let thinking = self.pending[..pos].to_string();
                        if !thinking.trim().is_empty() {
                            deltas.push(ParsedDelta::Thinking(thinking));
                        }
                        self.pending.drain(..pos + "</think>".len());
                        self.state = ThinkingState::Content;
                    } else {
                        let emit_len = safe_emit_len(&self.pending, "</think>", is_partial);
                        if emit_len == 0 {
                            break;
                        }
                        let thinking = self.pending[..emit_len].to_string();
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
                ThinkingState::Thinking => deltas.push(ParsedDelta::Thinking(remaining)),
            }
        }

        deltas
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
    for len in (1..=max).rev() {
        if buffer.ends_with(&marker[..len]) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[(&str, bool)], starts_in_thinking: bool) -> Vec<ParsedDelta> {
        let mut state = ChatStreamingState::new(starts_in_thinking);
        let mut deltas = Vec::new();
        for (chunk, partial) in chunks {
            deltas.extend(state.update(chunk, *partial));
        }
        deltas.extend(state.finish());
        deltas
    }

    #[test]
    fn detects_unclosed_prompt_thinking_block() {
        assert!(prompt_starts_in_thinking("user<think>"));
        assert!(!prompt_starts_in_thinking("<think>x</think>done"));
    }

    #[test]
    fn swallows_empty_thinking_block() {
        let deltas = collect(&[("<think>\n\n</think>\n\nHello", true)], false);
        assert_eq!(deltas, vec![ParsedDelta::Content("\n\nHello".to_string())]);
    }

    #[test]
    fn handles_split_thinking_markers() {
        let deltas = collect(
            &[("<thi", true), ("nk>abc</thi", true), ("nk>Hi", true)],
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
        let deltas = collect(&[("<think>abc", true)], false);
        assert_eq!(deltas, vec![ParsedDelta::Thinking("abc".to_string())]);
    }

    #[test]
    fn starts_stream_in_thinking_when_prompt_left_open() {
        let deltas = collect(&[("thinking...</think>Hello", true)], true);
        assert_eq!(
            deltas,
            vec![
                ParsedDelta::Thinking("thinking...".to_string()),
                ParsedDelta::Content("Hello".to_string())
            ]
        );
    }
}
