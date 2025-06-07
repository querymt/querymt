use mistralrs::{
    CalledFunction, Function as MistralFunction, Tool as MistralTool, ToolCallResponse,
    ToolCallType, ToolChoice as MistralToolChoice, ToolType as MistralToolType,
};
use querymt::{ToolCall, error::LLMError};
use querymt::chat::{Tool, ToolChoice};

pub(crate) fn convert_tool_call(index: usize, call: &ToolCall) -> ToolCallResponse {
    ToolCallResponse {
        index,
        id: call.id.clone(),
        tp: ToolCallType::Function,
        function: CalledFunction {
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
        },
    }
}

pub(crate) fn build_mistral_tools(tools: &[Tool]) -> Result<Vec<MistralTool>, LLMError> {
    tools
        .iter()
        .map(|tool| {
            if tool.tool_type != "function" {
                return Err(LLMError::InvalidRequest(format!(
                    "unsupported tool type: {}",
                    tool.tool_type
                )));
            }
            let params = match tool.function.parameters.as_object() {
                Some(map) => Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
                None => None,
            };
            Ok(MistralTool {
                tp: MistralToolType::Function,
                function: MistralFunction {
                    description: Some(tool.function.description.clone()),
                    name: tool.function.name.clone(),
                    parameters: params,
                },
            })
        })
        .collect()
}

pub(crate) fn map_tool_choice(
    choice: &ToolChoice,
    tools: &[MistralTool],
) -> Result<MistralToolChoice, LLMError> {
    match choice {
        ToolChoice::Auto => Ok(MistralToolChoice::Auto),
        ToolChoice::None => Ok(MistralToolChoice::None),
        ToolChoice::Any => Ok(MistralToolChoice::Auto),
        ToolChoice::Tool(name) => {
            let tool = tools
                .iter()
                .find(|tool| tool.function.name == *name)
                .cloned()
                .ok_or_else(|| LLMError::ToolConfigError(format!("tool not found: {}", name)))?;
            Ok(MistralToolChoice::Tool(tool))
        }
    }
}
