use crate::{
    chat::{FunctionTool, Tool},
    tool_decorator::CallFunctionTool,
};
use anyhow::Result;
use async_trait::async_trait;
use jsonschema::ValidationError;
use rmcp::model::Tool as RmcpTool;
use rmcp::{model::CallToolRequestParam, service::ServerSink};
use serde_json::Value;
use std::convert::TryFrom;
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

    #[instrument(name = "from_mcp_tool", skip(r))]
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
        if let Some(output_schema) = r.output_schema {
            if let Ok(os_str) = serde_json::to_string(&output_schema) {
                description = format!("{}. Returns {}", description, os_str);
            }
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

#[async_trait]
impl CallFunctionTool for McpToolAdapter {
    fn descriptor(&self) -> Tool {
        self.tool.clone()
    }

    fn server_name(&self) -> Option<&str> {
        Some(&self.server_name)
    }

    #[instrument(name = "mcp_tool.call", skip_all, fields(name = %self.mcp_tool.name))]
    async fn call(&self, args: Value) -> Result<String> {
        let arguments = match args {
            Value::Object(map) => Some(map),
            _ => None,
        };

        let call_result = self
            .server
            .call_tool(CallToolRequestParam {
                name: self.mcp_tool.name.clone(),
                arguments,
            })
            .await?;
        Ok(serde_json::to_string(&call_result)?)
    }
}
