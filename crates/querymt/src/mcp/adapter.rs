use crate::{
    chat::{Content, FunctionTool, Tool},
    tool_decorator::CallFunctionTool,
};
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine;
use jsonschema::ValidationError;
use rmcp::model::{RawContent, ResourceContents, Tool as RmcpTool};
use rmcp::{model::CallToolRequestParams, service::ServerSink};
use serde_json::Value;
use std::convert::TryFrom;
#[cfg(feature = "tracing")]
use tracing::instrument;

/// Error type for when the schema in the RMCP Tool doesn't match your ParametersSchema.
#[derive(thiserror::Error, Debug)]
pub enum AdapterError {
    #[error("failed to serialize RMCP schema to JSON for tool `{tool_name}`: {source}")]
    SerdeJson {
        tool_name: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to validate RMCP schema for tool `{tool_name}`: {source}")]
    ValidationError {
        tool_name: String,
        #[source] // Keep the original error!
        source: jsonschema::ValidationError<'static>,
    },
}

impl TryFrom<RmcpTool> for FunctionTool {
    type Error = AdapterError;

    #[cfg_attr(feature = "tracing", instrument(name = "from_mcp_tool", skip(r)))]
    fn try_from(r: RmcpTool) -> Result<Self, Self::Error> {
        let tool_name = r.name.to_string();
        log::debug!("adding mcp tool: {}", tool_name);

        let schema = r.schema_as_json_value();
        jsonschema::draft202012::meta::validate(&schema).map_err(|err| {
            // Because lifetimes can be tricky, we take ownership of the error
            let static_err: ValidationError<'static> = err.to_owned();
            AdapterError::ValidationError {
                tool_name: tool_name.clone(), // We need the tool_name here
                source: static_err,
            }
        })?;

        let mut description = r.description.clone().unwrap_or_default().to_string();
        if let Some(output_schema) = r.output_schema
            && let Ok(os_str) = serde_json::to_string(&output_schema)
        {
            description = format!("{}. Returns {}", description, os_str);
        }

        Ok(FunctionTool {
            name: tool_name,
            description,
            parameters: schema,
        })
    }
}

impl TryFrom<RmcpTool> for Tool {
    type Error = AdapterError;

    fn try_from(r: RmcpTool) -> Result<Self, Self::Error> {
        let func = FunctionTool::try_from(r)?;
        Ok(Tool {
            tool_type: "function".to_string(), // or whatever you want here
            function: func,
        })
    }
}

pub struct McpToolAdapter {
    mcp_tool: RmcpTool,
    server: ServerSink,
    server_name: String,
    tool: Tool,
}

impl McpToolAdapter {
    pub fn try_new(
        mcp_tool: RmcpTool,
        server: ServerSink,
        server_name: String,
    ) -> Result<Self, Box<AdapterError>> {
        let tool = Tool::try_from(mcp_tool.clone())?;
        Ok(Self {
            mcp_tool,
            server,
            server_name,
            tool,
        })
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }
}

/// Convert an MCP `RawContent` value into a `Content` block.
impl From<RawContent> for Content {
    fn from(raw: RawContent) -> Self {
        let b64 = base64::engine::general_purpose::STANDARD;
        match raw {
            RawContent::Text(t) => Content::text(t.text),
            RawContent::Image(img) => Content::Image {
                mime_type: img.mime_type,
                data: b64.decode(&img.data).unwrap_or_default(),
            },
            RawContent::Audio(a) => Content::Audio {
                mime_type: a.mime_type,
                data: b64.decode(&a.data).unwrap_or_default(),
            },
            RawContent::Resource(r) => match r.resource {
                ResourceContents::TextResourceContents { text, .. } => Content::text(text),
                ResourceContents::BlobResourceContents {
                    blob, mime_type, ..
                } => Content::Image {
                    mime_type: mime_type.unwrap_or_default(),
                    data: b64.decode(&blob).unwrap_or_default(),
                },
            },
            RawContent::ResourceLink(r) => Content::ResourceLink {
                uri: r.uri,
                name: Some(r.name),
                description: r.description,
                mime_type: r.mime_type,
            },
        }
    }
}

#[async_trait]
impl CallFunctionTool for McpToolAdapter {
    fn descriptor(&self) -> Tool {
        self.tool.clone()
    }

    fn server_name(&self) -> Option<&str> {
        Some(&self.server_name)
    }

    #[cfg_attr(feature = "tracing", instrument(name = "mcp_tool.call", skip_all, fields(name = %self.mcp_tool.name)))]
    async fn call(&self, args: Value) -> Result<Vec<Content>> {
        let arguments = match args {
            Value::Object(map) => Some(map),
            _ => None,
        };

        let mut params = CallToolRequestParams::new(self.mcp_tool.name.clone());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        let call_result = self.server.call_tool(params).await?;
        Ok(call_result
            .content
            .into_iter()
            .map(|c| Content::from(c.raw))
            .collect())
    }
}
