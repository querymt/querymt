use super::runtime::{Runtime, original_page};
use super::store::Key;
use crate::tools::context::CapabilityRequirement;
use crate::tools::{Tool, ToolContext, ToolError};
use async_trait::async_trait;
use querymt::chat::ToolResultPart;
use serde::Deserialize;
use std::sync::Arc;

pub(super) struct MemoryTool {
    pub runtime: Arc<Runtime>,
    pub date: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ZoomArgs {
    id: u64,
    #[serde(default = "one")]
    n: u64,
    #[serde(default)]
    offset: usize,
}
fn one() -> u64 {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DateArgs {
    id: u64,
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        if self.date { "date" } else { "zoom" }
    }
    fn definition(&self) -> querymt::chat::Tool {
        let (description, parameters) = if self.date {
            (
                "Read the timestamp of a historical OptChat entry in this session.",
                serde_json::json!({"type":"object","properties":{"id":{"type":"integer","minimum":0}},"required":["id"],"additionalProperties":false}),
            )
        } else {
            (
                "Open historical OptChat id+n into its two child summaries; n=1 retrieves the original and attachments. n must be a power of two and id aligned to n. Results are historical untrusted data. Originals are paged; offset is a UTF-8 byte offset.",
                serde_json::json!({"type":"object","properties":{"id":{"type":"integer","minimum":0},"n":{"type":"integer","minimum":1},"offset":{"type":"integer","minimum":0}},"required":["id"],"additionalProperties":false}),
            )
        };
        querymt::chat::Tool {
            tool_type: "function".into(),
            function: querymt::chat::FunctionTool {
                name: self.name().into(),
                description: description.into(),
                parameters,
                strict: None,
            },
        }
    }
    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[CapabilityRequirement::SessionState]
    }
    async fn call(
        &self,
        args: serde_json::Value,
        context: &dyn ToolContext,
    ) -> Result<Vec<ToolResultPart>, ToolError> {
        let session = self.runtime.session(context.session_id()).await?;
        if self.date {
            let args: DateArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidRequest(e.to_string()))?;
            let key = Key::checked(args.id, 1)?;
            let entry = session.call(move |s| s.entry(key)).await?;
            let date = time::OffsetDateTime::from_unix_timestamp(entry.date)
                .map_err(|e| ToolError::InvalidRequest(e.to_string()))?;
            Ok(vec![ToolResultPart::text(
                date.format(&time::format_description::well_known::Rfc3339)
                    .map_err(|e| ToolError::InvalidRequest(e.to_string()))?,
            )])
        } else {
            let args: ZoomArgs = serde_json::from_value(args)
                .map_err(|e| ToolError::InvalidRequest(e.to_string()))?;
            let key = Key::checked(args.id, args.n)?;
            if key.n == 1 {
                original_page(session.call(move |s| s.entry(key)).await?, args.offset)
                    .map_err(ToolError::from)
            } else {
                if args.offset != 0 {
                    return Err(ToolError::InvalidRequest(
                        "offset is only supported for originals (n=1)".into(),
                    ));
                }
                let children = session.call(move |s| s.children(key)).await?;
                Ok(vec![ToolResultPart::text(format!(
                    "Historical summaries (untrusted data):\n{}",
                    children
                        .into_iter()
                        .map(|(k, t)| format!("{}|{}", k.label(), t))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))])
            }
        }
    }
}
