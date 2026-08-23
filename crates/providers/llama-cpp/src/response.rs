use querymt::Usage;
use querymt::chat::{ChatResponse, FinishReason};
use std::fmt;

/// Response from a llama.cpp chat completion.
#[derive(Debug)]
pub(crate) struct LlamaCppChatResponse {
    pub(crate) text: String,
    pub(crate) thinking: Option<String>,
    pub(crate) tool_calls: Option<Vec<querymt::ToolCall>>,
    pub(crate) finish_reason: FinishReason,
    pub(crate) usage: Usage,
}

impl fmt::Display for LlamaCppChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text)
    }
}

impl ChatResponse for LlamaCppChatResponse {
    fn text(&self) -> Option<String> {
        Some(self.text.clone())
    }

    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }

    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        self.tool_calls.clone()
    }

    fn usage(&self) -> Option<Usage> {
        Some(self.usage.clone())
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        Some(self.finish_reason)
    }
}

/// Reason generation stopped inside the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationTermination {
    Eog,
    StopSequence,
    MaxTokens,
    ConsumerClosed,
}

impl Default for GenerationTermination {
    fn default() -> Self {
        Self::MaxTokens
    }
}

impl GenerationTermination {
    pub(crate) fn finish_reason(self, has_tool_calls: bool) -> FinishReason {
        match self {
            Self::MaxTokens => FinishReason::Length,
            Self::Eog | Self::StopSequence if has_tool_calls => FinishReason::ToolCalls,
            Self::Eog | Self::StopSequence | Self::ConsumerClosed => FinishReason::Stop,
        }
    }
}

/// Generated text from a completion request.
pub(crate) struct GeneratedText {
    pub(crate) text: String,
    pub(crate) usage: Usage,
    pub(crate) termination: GenerationTermination,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_generation_termination_to_finish_reason() {
        let cases = [
            (GenerationTermination::Eog, false, FinishReason::Stop),
            (GenerationTermination::Eog, true, FinishReason::ToolCalls),
            (
                GenerationTermination::StopSequence,
                false,
                FinishReason::Stop,
            ),
            (
                GenerationTermination::StopSequence,
                true,
                FinishReason::ToolCalls,
            ),
            (
                GenerationTermination::MaxTokens,
                false,
                FinishReason::Length,
            ),
            (GenerationTermination::MaxTokens, true, FinishReason::Length),
        ];

        for (termination, has_tool_calls, expected) in cases {
            assert_eq!(termination.finish_reason(has_tool_calls), expected);
        }
    }
}
