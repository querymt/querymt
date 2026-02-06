//! Question tool for asking users structured questions

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tools::{Tool, ToolContext, ToolError};

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
                description: "Ask the user one or more structured questions and wait for answers. This tool blocks until the user responds. Use this when you need to gather user preferences, requirements, or decisions during execution."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "Questions to ask the user",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": {
                                        "type": "string",
                                        "description": "The complete question to ask"
                                    },
                                    "header": {
                                        "type": "string",
                                        "description": "Short header label (max 12 chars)",
                                        "maxLength": 12
                                    },
                                    "options": {
                                        "type": "array",
                                        "description": "Available choices",
                                        "items": {
                                            "type": "object",
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
            },
        }
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
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
                    &question.header,
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
            .map_err(|e| ToolError::ProviderError(format!("Failed to serialize result: {}", e)))
    }
}
