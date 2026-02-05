//! mdq tool integration.
//!
//! Runs an mdq selector against a markdown file and returns JSON.

use async_trait::async_trait;
use querymt::chat::{FunctionTool, Tool};
use serde_json::{Value, json};

use crate::tools::{CapabilityRequirement, Tool as ToolTrait, ToolContext, ToolError};

pub struct MdqTool;

impl Default for MdqTool {
    fn default() -> Self {
        Self::new()
    }
}

impl MdqTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ToolTrait for MdqTool {
    fn name(&self) -> &str {
        "mdq"
    }

    fn definition(&self) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: self.name().to_string(),
                description: "Select elements from a markdown file using an mdq selector (selectors can be combined with `|`). Returns matched nodes as JSON."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the markdown file (relative to cwd or absolute)."
                        },
                        "selector": {
                            "type": "string",
                            "description": r#"mdq selector = Markdown-shaped filters. Combine selectors with `|`:
- Sections: `# heading`
- Lists: `- text` (unordered), `1. text` (ordered)
- Tasks: `- [ ] text` (open), `- [x] text` (done), `- [?] text` (any)
- Links/images: `[]()` (link), `![]()` (image)
- Quotes/code/html: `> text` (blockquote), ``` ```lang text ``` (code block), `</> tag` (raw HTML)
- Paragraphs/tables/front matter: `P: text`, `:-: hdr :-: row`, `+++[toml|yaml] text`
Text matching: unquoted = case-insensitive; quoted = case-sensitive; `^...$` anchors; `/regex/`; `*` or omitted = match any."#                        }
                    },
                    "required": ["path", "selector"]
                }),
            },
        }
    }

    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::Filesystem]
    }

    async fn call(&self, args: Value, context: &dyn ToolContext) -> Result<String, ToolError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("path is required".to_string()))?;
        let selector_str = args
            .get("selector")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidRequest("selector is required".to_string()))?;

        let resolved_path = context.resolve_path(path)?;
        let markdown = tokio::fs::read_to_string(&resolved_path)
            .await
            .map_err(|e| ToolError::ProviderError(format!("read failed: {}", e)))?;

        let parse_options = mdq::md_elem::ParseOptions::gfm();
        let md_doc = mdq::md_elem::MdDoc::parse(&markdown, &parse_options)
            .map_err(|e| ToolError::ProviderError(format!("markdown parse failed: {e}")))?;

        let selector: mdq::select::Selector = selector_str
            .try_into()
            .map_err(|e| ToolError::InvalidRequest(format!("invalid selector: {e}")))?;

        let (found_nodes, ctx) = selector
            .find_nodes(md_doc)
            .map_err(|e| ToolError::ProviderError(format!("selection failed: {e}")))?;

        let found_any = !found_nodes.is_empty();
        let md_json = serde_json::to_value(&mdq::output::SerializableMd::new(
            &found_nodes,
            &ctx,
            mdq::output::InlineElemOptions::default(),
        ))
        .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))?;

        let result = json!({
            "path": resolved_path.display().to_string(),
            "selector": selector_str,
            "found_any": found_any,
            "matched_count": found_nodes.len(),
            "md": md_json
        });

        serde_json::to_string(&result)
            .map_err(|e| ToolError::ProviderError(format!("serialize failed: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::AgentToolContext;
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn selects_nodes_from_markdown_file() {
        let temp_dir = TempDir::new().unwrap();
        let md_path = temp_dir.path().join("doc.md");
        let mut f = fs::File::create(&md_path).unwrap();
        writeln!(
            f,
            "## First section\n\n- hello\n- world\n\n## Second section\n\n- foo\n- bar\n"
        )
        .unwrap();

        let context =
            AgentToolContext::basic("test".to_string(), Some(temp_dir.path().to_path_buf()));
        let tool = MdqTool::new();

        let args = json!({
            "path": "doc.md",
            "selector": "# second | - *"
        });

        let result = tool.call(args, &context).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["found_any"].as_bool().unwrap(), true);
        assert_eq!(parsed["matched_count"].as_u64().unwrap(), 2);
        assert!(parsed["md"].get("items").is_some());
    }
}
