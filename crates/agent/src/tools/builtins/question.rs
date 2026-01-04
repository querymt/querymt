//! Question tool for asking users structured questions

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool as ChatTool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, Write};

use crate::tools::{Tool, ToolContext, ToolError};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QuestionOption {
    label: String,
    description: String,
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

    fn ask_question_interactive(question: &QuestionInfo) -> Result<Vec<String>, String> {
        println!("\n{}", "=".repeat(60));
        println!("{}", question.header);
        println!("{}", "=".repeat(60));
        println!("{}\n", question.question);

        for (idx, option) in question.options.iter().enumerate() {
            println!("{}. {} - {}", idx + 1, option.label, option.description);
        }

        if question.multiple {
            println!(
                "\nEnter your choices (comma-separated numbers, or 'other' for custom input): "
            );
        } else {
            println!("\nEnter your choice (number, or 'other' for custom input): ");
        }

        print!("> ");
        io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;
        let input = input.trim();

        if input.to_lowercase() == "other" {
            println!("Enter your custom response: ");
            print!("> ");
            io::stdout().flush().map_err(|e| e.to_string())?;

            let mut custom = String::new();
            io::stdin()
                .read_line(&mut custom)
                .map_err(|e| e.to_string())?;
            return Ok(vec![custom.trim().to_string()]);
        }

        let selections: Vec<usize> = input
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .collect();

        let mut answers = Vec::new();
        for sel in selections {
            if sel > 0 && sel <= question.options.len() {
                answers.push(question.options[sel - 1].label.clone());
            }
        }

        if answers.is_empty() {
            Err("No valid selections made".to_string())
        } else {
            Ok(answers)
        }
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

    async fn call(&self, args: Value, _context: &dyn ToolContext) -> Result<String, ToolError> {
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

        for question in questions {
            let answers = tokio::task::spawn_blocking({
                let q = question.clone();
                move || Self::ask_question_interactive(&q)
            })
            .await
            .map_err(|e| ToolError::ProviderError(format!("Question task failed: {}", e)))?
            .map_err(|e| ToolError::ProviderError(format!("Failed to get answer: {}", e)))?;

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
