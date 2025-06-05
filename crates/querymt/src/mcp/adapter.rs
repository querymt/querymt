use crate::{
    chat::{FunctionTool, Tool},
    tool_decorator::CallFunctionTool,
};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::CallToolRequestParam;
use rmcp::model::Tool as RmcpTool;
use rmcp::service::ServerSink;
use serde_json::Value;
use std::convert::TryFrom;

/// Error type for when the schema in the RMCP Tool doesn't match your ParametersSchema.
#[derive(thiserror::Error, Debug)]
pub enum AdapterError {
    #[error("failed to serialize RMCP schema to JSON for tool `{tool_name}`: {source}")]
    SerdeJson {
        tool_name: String,
        #[source]
        source: serde_json::Error,
    },
}

impl TryFrom<RmcpTool> for FunctionTool {
    type Error = AdapterError;

    fn try_from(r: RmcpTool) -> Result<Self, Self::Error> {
        let tool_name = r.name.to_string();
        log::debug!("adding mcp tool: {}", tool_name);
        Ok(FunctionTool {
            name: tool_name,
            description: r.description.to_string(),
            parameters: r.schema_as_json_value(),
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
    tool: Tool,
}

impl McpToolAdapter {
    pub fn try_new(mcp_tool: RmcpTool, server: ServerSink) -> Result<Self, AdapterError> {
        let tool = Tool::try_from(mcp_tool.clone())?;
        Ok(Self {
            mcp_tool,
            server,
            tool,
        })
    }
}

#[async_trait]
impl CallFunctionTool for McpToolAdapter {
    fn descriptor(&self) -> Tool {
        self.tool.clone()
    }

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
