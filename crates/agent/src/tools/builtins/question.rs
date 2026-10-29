//! Question tool for asking users structured questions

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool, ToolResultPart};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tools::{Tool, ToolContext, ToolError, ToolExecutionClass};

/// A single option in a question
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QuestionInfo {
    question: String,
    header: String,
    options: Vec<QuestionOption>,
    #[serde(default)]
    multiple: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct QuestionAnswer {
    question: String,
    answers: Vec<String>,
}

/// Longest header shown to the user, in Unicode scalar values.
const MAX_HEADER_CHARS: usize = 12;

/// Shorten `header` to its first [`MAX_HEADER_CHARS`] Unicode scalar values.
///
/// Cuts on scalar boundaries, so the result is always valid UTF-8. Applies to
/// the header only; no other argument is truncated.
fn truncate_header(header: &str) -> &str {
    match header.char_indices().nth(MAX_HEADER_CHARS) {
        Some((end, _)) => &header[..end],
        None => header,
    }
}

/// Question tool for interactive Q&A with users
pub struct QuestionTool;

impl QuestionTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for QuestionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for QuestionTool {
    fn name(&self) -> &str {
        "question"
    }

    fn definition(&self) -> ChatTool {
        ChatTool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Ask the user one or more structured questions and wait for answers. This tool blocks until the user responds. Use this when you need to gather user preferences, requirements, or decisions during execution. The user is not limited to the listed options: they may reply with free-form text instead of picking a choice."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "Questions to ask the user",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "question": {
                                        "type": "string",
                                        "description": "The complete question to ask"
                                    },
                                    "header": {
                                        "type": "string",
                                        "description": "Short header label (keep it concise: at most 12 characters; longer values are shortened)"
                                    },
                                    "options": {
                                        "type": "array",
                                        "description": "Available choices",
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "properties": {
                                                "label": {
                                                    "type": "string",
                                                    "description": "Display text (1-5 words, concise)"
                                                },
                                                "description": {
                                                    "type": "string",
                                                    "description": "Explanation of this choice"
                                                }
                                            },
                                            "required": ["label", "description"]
                                        }
                                    },
                                    "multiple": {
                                        "type": "boolean",
                                        "description": "Allow selecting multiple choices (default false)",
                                        "default": false
                                    }
                                },
                                "required": ["question", "header", "options"]
                            }
                        }
                    },
                    "required": ["questions"]
                }),
                strict: None,
            },
        }
    }

    fn execution_class(&self) -> ToolExecutionClass {
        ToolExecutionClass::ClarificationBoundary
    }

    async fn call(
        &self,
        args: Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let questions_val = args
            .get("questions")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidRequest("questions array is required".to_string()))?;

        let questions: Vec<QuestionInfo> = questions_val
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ToolError::InvalidRequest(format!("Invalid question format: {}", e)))?;

        let mut all_answers = Vec::new();

        for (idx, question) in questions.into_iter().enumerate() {
            // Create a unique question ID for this question
            let question_id = format!("question_{}_{}", uuid::Uuid::new_v4(), idx);

            // Convert QuestionOption to (label, description) pairs
            let options: Vec<(String, String)> = question
                .options
                .iter()
                .map(|opt| (opt.label.clone(), opt.description.clone()))
                .collect();

            // Use context.ask_question() which works across all transports
            let answers = context
                .ask_question(
                    &question_id,
                    &question.question,
                    truncate_header(&question.header),
                    &options,
                    question.multiple,
                )
                .await?;

            all_answers.push(QuestionAnswer {
                question: question.question.clone(),
                answers,
            });
        }

        let result = json!({
            "answers": all_answers,
        });

        serde_json::to_string_pretty(&result)
            .map(|s| vec![ToolResultPart::text(s)])
            .map_err(|e| ToolError::ProviderError(format!("Failed to serialize result: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elicitation::{ElicitationAction, ElicitationResponse};
    use crate::tools::AgentToolContext;
    use tokio::sync::mpsc;

    /// Run the tool against a context that answers every elicitation, returning
    /// the headers the user would have been shown.
    async fn shown_headers(args: Value) -> Vec<String> {
        let (tx, mut rx) = mpsc::channel(4);
        let context = AgentToolContext::new("test".to_string(), None, None, Some(tx));
        let responder = tokio::spawn(async move {
            let mut headers = Vec::new();
            while let Some(request) = rx.recv().await {
                let selection = &request.requested_schema["properties"]["selection"];
                headers.push(selection["title"].as_str().unwrap().to_string());
                let _ = request.response_tx.send(ElicitationResponse {
                    action: ElicitationAction::Accept,
                    content: Some(json!({"selection": "A"})),
                });
            }
            headers
        });
        QuestionTool::new().call(args, &context).await.unwrap();
        drop(context);
        responder.await.unwrap()
    }

    fn question_args(header: &str) -> Value {
        json!({"questions": [{
            "question": "Which one?",
            "header": header,
            "options": [{"label": "A", "description": "first"}],
        }]})
    }

    #[test]
    fn truncate_header_cuts_on_scalar_boundaries() {
        assert_eq!(truncate_header(""), "");
        assert_eq!(truncate_header("short"), "short");
        assert_eq!(truncate_header("exactly12chr"), "exactly12chr");
        assert_eq!(truncate_header("Deployability"), "Deployabilit");
        // 13 three-byte scalars: cut after the 12th, never inside a character.
        let wide = "日".repeat(13);
        assert_eq!(truncate_header(&wide), "日".repeat(12));
        // Combining marks and emoji count as scalar values, not grapheme clusters.
        let mixed = "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}";
        assert_eq!(truncate_header(mixed).chars().count(), 12);
    }

    #[test]
    fn schema_accepts_overlong_headers_but_still_requires_a_string() {
        let schema = QuestionTool::new().definition().function.parameters;
        assert!(
            !schema.to_string().contains("maxLength"),
            "header length must not be a validation rule: {schema}"
        );
        let validator = jsonschema::validator_for(&schema).unwrap();
        for header in ["Deployability", &"x".repeat(500), &"日".repeat(13)] {
            assert!(validator.is_valid(&question_args(header)), "{header}");
        }
        let mut bad = question_args("ok");
        bad["questions"][0]["header"] = json!(5);
        assert!(!validator.is_valid(&bad));
    }

    #[tokio::test]
    async fn overlong_ascii_header_is_shortened_before_presentation() {
        let headers = shown_headers(question_args("Deployability")).await;
        assert_eq!(headers, vec!["Deployabilit"]);
    }

    #[tokio::test]
    async fn overlong_multibyte_header_keeps_valid_utf8() {
        let header = "日本語日本語日本語日本語日";
        assert_eq!(header.chars().count(), 13);
        let headers = shown_headers(question_args(header)).await;
        assert_eq!(headers, vec!["日本語日本語日本語日本語"]);
        assert_eq!(headers[0].chars().count(), 12);
    }

    #[tokio::test]
    async fn short_header_and_other_strings_are_untouched() {
        let long_question = "q".repeat(300);
        let args = json!({"questions": [{
            "question": long_question,
            "header": "Short",
            "options": [{"label": "A label that is quite long indeed", "description": "d"}],
        }]});
        let (tx, mut rx) = mpsc::channel(1);
        let context = AgentToolContext::new("test".to_string(), None, None, Some(tx));
        let responder = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            let seen = (
                request.message.clone(),
                request.requested_schema["properties"]["selection"]["title"].clone(),
                request.requested_schema["properties"]["selection"]["oneOf"][0]["const"].clone(),
            );
            let _ = request.response_tx.send(ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(json!({"selection": "x"})),
            });
            seen
        });
        QuestionTool::new().call(args, &context).await.unwrap();
        let (message, title, label) = responder.await.unwrap();
        assert_eq!(message.len(), 300);
        assert_eq!(title, json!("Short"));
        assert_eq!(label, json!("A label that is quite long indeed"));
    }
}
