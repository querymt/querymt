use crate::{
    chat::{FunctionTool, ParametersSchema, Tool},
    tool_decorator::CallFunctionTool,
};
use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::CallToolRequestParam;
use rmcp::model::Tool as RmcpTool;
use rmcp::service::ServerSink;
use serde_json::{Map, Value};
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
        let tool_name = r.name.into_owned();

        let mut schema_map: Map<String, Value> = Map::new();
        for (k, v) in r.input_schema.as_ref().iter() {
            schema_map.insert(k.clone(), v.clone());
        }

        let params: ParametersSchema =
            serde_json::from_value(Value::Object(schema_map)).map_err(|e| {
                AdapterError::SerdeJson {
                    tool_name: tool_name.clone(),
                    source: e,
                }
            })?;

        Ok(FunctionTool {
            name: tool_name,
            description: r.description.into_owned(),
            parameters: params,
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
