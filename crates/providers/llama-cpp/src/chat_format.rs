use crate::common_chat::{GrammarTrigger, ReasoningFormat, ToolGrammar};
use querymt::chat::Tool;
use querymt::{FunctionCall, ToolCall};
use serde_json::{Map, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub(crate) struct ParsedFormat {
    pub content: String,
    pub thinking: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) enum ParsedDelta {
    Content(String),
    Thinking(String),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ToolFormat {
    Qwen3,
    QwenFunction,
    Ministral3,
    Lfm2,
    FunctionGemma,
    Gemma4,
}

impl ToolFormat {
    pub(crate) fn detect(
        template: &str,
        architecture: Option<&str>,
        model_name: Option<&str>,
    ) -> Option<Self> {
        if template.contains("<|tool_call_start|>") {
            Some(Self::Lfm2)
        } else if template.contains("[TOOL_CALLS]") || template.contains("[ARGS]") {
            Some(Self::Ministral3)
        } else if template.contains("<start_function_call>") {
            Some(Self::FunctionGemma)
        } else if template.contains("<|tool_call>") || template.contains("<tool_call|>") {
            Some(Self::Gemma4)
        } else if template.contains("<function=") || template.contains("<parameter=") {
            Some(Self::QwenFunction)
        } else if template.contains("<tool_call>") && template.contains("arguments") {
            Some(Self::Qwen3)
        } else {
            detect_from_hints(architecture, model_name)
        }
    }

    pub(crate) fn grammar(self, tools: &[Tool]) -> Option<ToolGrammar> {
        match self {
            Self::Qwen3 => qwen3_tool_grammar(tools),
            Self::QwenFunction => qwen_function_tool_grammar(tools),
            Self::Ministral3 => ministral_tool_grammar(tools),
            Self::Lfm2 => lfm2_tool_grammar(tools),
            Self::FunctionGemma => function_gemma_tool_grammar(tools),
            Self::Gemma4 => gemma4_tool_grammar(tools),
        }
    }
}

#[cfg(test)]
pub(crate) fn parse_assistant_format(text: &str) -> ParsedFormat {
    parse_assistant_format_with_state(text, ReasoningFormat::ThinkTags, false)
}

pub(crate) fn parse_assistant_format_with_state(
    text: &str,
    reasoning_format: ReasoningFormat,
    starts_in_thinking: bool,
) -> ParsedFormat {
    let (thinking, without_thinking) =
        extract_reasoning_blocks(text, reasoning_format, starts_in_thinking);
    let calls = extract_tool_calls(&without_thinking);
    let content = strip_tool_blocks(&without_thinking).trim().to_string();

    ParsedFormat {
        content,
        thinking,
        tool_calls: calls,
    }
}

fn detect_from_hints(architecture: Option<&str>, model_name: Option<&str>) -> Option<ToolFormat> {
    let architecture = architecture.unwrap_or_default().to_ascii_lowercase();
    let model_name = model_name.unwrap_or_default().to_ascii_lowercase();
    let combined = format!("{architecture} {model_name}");

    if combined.contains("functiongemma") || combined.contains("function-gemma") {
        return Some(ToolFormat::FunctionGemma);
    }
    if combined.contains("gemma-4") || combined.contains("gemma4") {
        return Some(ToolFormat::Gemma4);
    }
    if combined.contains("lfm") {
        return Some(ToolFormat::Lfm2);
    }
    if combined.contains("ministral") {
        return Some(ToolFormat::Ministral3);
    }
    if combined.contains("qwen3.5")
        || combined.contains("qwen3.6")
        || combined.contains("qwen 3.5")
        || combined.contains("qwen 3.6")
        || combined.contains("qwen-3.5")
        || combined.contains("qwen-3.6")
        || combined.contains("qwen35")
        || combined.contains("qwen36")
    {
        return Some(ToolFormat::QwenFunction);
    }
    if combined.contains("qwen3") || combined.contains("qwen 3") || combined.contains("qwen-3") {
        return Some(ToolFormat::Qwen3);
    }
    None
}

fn extract_reasoning_blocks(
    text: &str,
    reasoning_format: ReasoningFormat,
    starts_in_thinking: bool,
) -> (Option<String>, String) {
    let open_tag = reasoning_format.open_tag();
    let close_tag = reasoning_format.close_tag();
    let mut rest = text;
    let mut output = String::with_capacity(text.len());
    let mut reasoning = String::new();
    let mut in_thinking = starts_in_thinking;

    if starts_in_thinking {
        if let ReasoningFormat::GemmaChannel {
            implicit_leading_reasoning_prefix: true,
        } = reasoning_format
        {
            rest = rest.strip_prefix("thought\n").unwrap_or(rest);
        }
    }

    while !rest.is_empty() {
        if in_thinking {
            if let Some(end) = rest.find(close_tag) {
                let block = reasoning_format.strip_reasoning_prefix(rest[..end].trim());
                if !reasoning.is_empty() && !block.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(block.trim());
                rest = &rest[end + close_tag.len()..];
                in_thinking = false;
            } else {
                let block = reasoning_format.strip_reasoning_prefix(rest.trim());
                if !reasoning.is_empty() && !block.is_empty() {
                    reasoning.push('\n');
                }
                reasoning.push_str(block.trim());
                rest = "";
            }
            continue;
        }

        if let Some(start) = rest.find(open_tag) {
            output.push_str(&rest[..start]);
            rest = &rest[start + open_tag.len()..];
            in_thinking = true;
        } else {
            output.push_str(rest);
            break;
        }
    }

    let thinking = (!reasoning.trim().is_empty()).then(|| reasoning.trim().to_string());
    (thinking, output)
}

fn extract_tool_calls(text: &str) -> Option<Vec<ToolCall>> {
    let mut calls = Vec::new();
    calls.extend(extract_qwen_json_tool_calls(text));
    calls.extend(extract_qwen_function_tool_calls(text));
    calls.extend(extract_ministral_tool_calls(text));
    calls.extend(extract_lfm2_tool_calls(text));
    calls.extend(extract_function_gemma_tool_calls(text));
    calls.extend(extract_gemma4_tool_calls(text));
    (!calls.is_empty()).then_some(calls)
}

fn tool_names_rule(tools: &[Tool]) -> Option<String> {
    (!tools.is_empty()).then(|| {
        tools
            .iter()
            .map(|tool| gbnf_literal(&tool.function.name))
            .collect::<Vec<_>>()
            .join(" | ")
    })
}

fn qwen3_tool_grammar(tools: &[Tool]) -> Option<ToolGrammar> {
    let names = tool_names_rule(tools)?;
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= qwen-tool-call+
qwen-tool-call ::= "<tool_call>" ws "{{" ws "\"name\"" ws ":" ws tool-name ws "," ws "\"arguments\"" ws ":" ws object ws "}}" ws "</tool_call>" ws

tool-name ::= {names}
{json_rules}"#,
            json_rules = json_gbnf_rules(),
        ),
        root: "root",
        lazy: true,
        triggers: word_trigger("<tool_call>"),
    })
}

fn qwen_function_tool_grammar(tools: &[Tool]) -> Option<ToolGrammar> {
    let tool_rules = qwen_function_tool_rules(tools)?;
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= toolcall+
toolcall ::= "<tool_call>\n" tool-alt "</tool_call>"
tool-alt ::= {tool_rules}
json-string ::= "\"" json-characters "\""
json-characters ::= ([^"\\] | "\\" json-escape)*
json-escape ::= ["\\/bfnrt] | "u" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]
json-number ::= "-"? ([0-9] | [1-9] [0-9]*) ("." [0-9]+)? ([eE] [+-]? [0-9]+)?
json-array ::= "[" ws (json-value (ws "," ws json-value)*)? ws "]"
json-member ::= json-string ws ":" ws json-value
json-object ::= "{{" ws (json-member (ws "," ws json-member)*)? ws "}}"
json-value ::= json-object | json-array | json-string | json-number | "true" | "false" | "null"
identifier ::= [A-Za-z_][A-Za-z0-9_-]*
value-text ::= [^<]*
ws ::= [ \t\n\r]*
{tool_specific_rules}"#,
            tool_rules = tool_rules.root_alternation,
            tool_specific_rules = tool_rules.rules,
        ),
        root: "root",
        lazy: true,
        triggers: word_trigger("<tool_call>"),
    })
}

fn ministral_tool_grammar(tools: &[Tool]) -> Option<ToolGrammar> {
    let names = tool_names_rule(tools)?;
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= ministral-call+
ministral-call ::= "[TOOL_CALLS]" tool-name "[ARGS]" object ws
tool-name ::= {names}
{json_rules}"#,
            json_rules = json_gbnf_rules(),
        ),
        root: "root",
        lazy: true,
        triggers: word_trigger("[TOOL_CALLS]"),
    })
}

fn lfm2_tool_grammar(tools: &[Tool]) -> Option<ToolGrammar> {
    let names = tool_names_rule(tools)?;
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= "<|tool_call_start|>" "[" lfm-call ("," ws lfm-call)* "]" "<|tool_call_end|>" ws
lfm-call ::= tool-name "(" ws (kwarg ("," ws kwarg)*)? ws ")"
kwarg ::= identifier ws "=" ws value
tool-name ::= {names}
identifier ::= [A-Za-z_][A-Za-z0-9_-]*
{json_rules}"#,
            json_rules = json_gbnf_rules(),
        ),
        root: "root",
        lazy: true,
        triggers: word_trigger("<|tool_call_start|>"),
    })
}

fn function_gemma_tool_grammar(tools: &[Tool]) -> Option<ToolGrammar> {
    let names = tool_names_rule(tools)?;
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= gemma-fn-call+
gemma-fn-call ::= "<start_function_call>" "call:" tool-name "{{" ws (gemma-param ("," ws gemma-param)*)? ws "}}" "<end_function_call>" ws
gemma-param ::= identifier ws ":" ws "<escape>" value-text "<escape>"
identifier ::= [A-Za-z_][A-Za-z0-9_-]*
value-text ::= [^<]*
tool-name ::= {names}
ws ::= [ \t\n\r]*
"#
        ),
        root: "root",
        lazy: true,
        triggers: word_trigger("<start_function_call>"),
    })
}

fn gemma4_tool_grammar(tools: &[Tool]) -> Option<ToolGrammar> {
    let names = tool_names_rule(tools)?;
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= gemma4-call+
gemma4-call ::= "<|tool_call>" "call:" tool-name gemma4-object "<tool_call|>" ws
gemma4-object ::= "{{" ws (gemma4-member ("," ws gemma4-member)*)? ws "}}"
gemma4-member ::= identifier ws ":" ws gemma4-value
gemma4-array ::= "[" ws (gemma4-value ("," ws gemma4-value)*)? ws "]"
gemma4-value ::= gemma4-object | gemma4-array | gemma4-string | number | "true" | "false" | "null"
gemma4-string ::= "<|\"|>" characters "<|\"|>"
identifier ::= [A-Za-z_][A-Za-z0-9_-]*
tool-name ::= {names}
characters ::= ([^"\\] | "\\" escape)*
escape ::= ["\\/bfnrt] | "u" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]
number ::= "-"? ([0-9] | [1-9] [0-9]*) ("." [0-9]+)? ([eE] [+-]? [0-9]+)?
ws ::= [ \t\n\r]*
"#,
        ),
        root: "root",
        lazy: true,
        triggers: word_trigger("<|tool_call>"),
    })
}

struct ToolRuleSet {
    root_alternation: String,
    rules: String,
}

fn qwen_function_tool_rules(tools: &[Tool]) -> Option<ToolRuleSet> {
    if tools.is_empty() {
        return None;
    }

    let mut root_rules = Vec::with_capacity(tools.len());
    let mut rule_blocks = Vec::new();

    for (tool_index, tool) in tools.iter().enumerate() {
        let prefix = format!("qwen-fn-{tool_index}");
        let call_rule = format!("{prefix}-call");
        let schema = &tool.function.parameters;
        let required = required_params(schema);
        let mut parameter_refs = Vec::new();

        if let Some(props) = schema.get("properties").and_then(|v| v.as_object()) {
            for (param_name, param_schema) in props {
                let param_rule = format!("{prefix}-{}-param", sanitize_rule_name(param_name));
                let value_rule = format!("{prefix}-{}-value", sanitize_rule_name(param_name));
                let value_expr = parameter_value_expr(param_schema, &value_rule);
                let block = format!(
                    "{value_rule} ::= {value_expr}\n{param_rule} ::= \"<parameter={param_name}>\\n\" {value_rule} \"\\n</parameter>\\n\""
                );
                rule_blocks.push(block);
                if required.contains(param_name.as_str()) {
                    parameter_refs.push(param_rule);
                } else {
                    parameter_refs.push(format!("({param_rule})?"));
                }
            }
        }

        let params = if parameter_refs.is_empty() {
            String::new()
        } else {
            format!(" {}", parameter_refs.join(" "))
        };
        rule_blocks.push(format!(
            "{call_rule} ::= \"<function={}>\\n\"{params} \"</function>\\n\"",
            tool.function.name
        ));
        root_rules.push(call_rule);
    }

    Some(ToolRuleSet {
        root_alternation: root_rules.join(" | "),
        rules: rule_blocks.join("\n"),
    })
}

fn required_params(schema: &Value) -> HashSet<&str> {
    schema
        .get("required")
        .and_then(|value| value.as_array())
        .map(|items| items.iter().filter_map(|value| value.as_str()).collect())
        .unwrap_or_default()
}

fn parameter_value_expr(schema: &Value, rule_name: &str) -> String {
    match schema.get("type").and_then(|value| value.as_str()) {
        Some("string") | None => {
            if let Some(values) = schema.get("enum").and_then(|value| value.as_array()) {
                let literals = values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .map(gbnf_literal)
                    .collect::<Vec<_>>();
                if !literals.is_empty() {
                    return literals.join(" | ");
                }
            }
            "value-text".to_string()
        }
        Some("integer") | Some("number") => "json-number".to_string(),
        Some("boolean") => "\"true\" | \"false\"".to_string(),
        Some("null") => "\"null\"".to_string(),
        Some("array") => "json-array".to_string(),
        Some("object") => "json-object".to_string(),
        Some(_) => rule_name.to_string(),
    }
}

fn sanitize_rule_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn json_gbnf_rules() -> &'static str {
    r#"object ::= "{" ws (member (ws "," ws member)*)? ws "}"
member ::= string ws ":" ws value
array ::= "[" ws (value (ws "," ws value)*)? ws "]"
value ::= object | array | string | number | "true" | "false" | "null"
string ::= "\"" characters "\""
characters ::= ([^"\\] | "\\" escape)*
escape ::= ["\\/bfnrt] | "u" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]
number ::= "-"? ([0-9] | [1-9] [0-9]*) ("." [0-9]+)? ([eE] [+-]? [0-9]+)?
ws ::= [ \t\n\r]*
"#
}

fn word_trigger(value: &str) -> Vec<GrammarTrigger> {
    vec![GrammarTrigger {
        value: value.to_string(),
    }]
}

fn gbnf_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn extract_qwen_json_tool_calls(text: &str) -> Vec<ToolCall> {
    extract_tag_bodies(text, "<tool_call>", "</tool_call>")
        .into_iter()
        .filter_map(|body| parse_json_tool_call(body.trim()))
        .collect()
}

fn parse_json_tool_call(raw: &str) -> Option<ToolCall> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let name = value.get("name")?.as_str()?.to_string();
    let arguments = value
        .get("arguments")
        .map(json_arguments_to_string)
        .unwrap_or_else(|| "{}".to_string());
    Some(ToolCall {
        id: stable_tool_call_id(&name, raw),
        call_type: "function".to_string(),
        function: FunctionCall { name, arguments },
    })
}

fn extract_qwen_function_tool_calls(text: &str) -> Vec<ToolCall> {
    extract_tag_bodies(text, "<tool_call>", "</tool_call>")
        .into_iter()
        .filter_map(parse_function_tool_call)
        .collect()
}

fn extract_ministral_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[TOOL_CALLS]") {
        let after = &rest[start + "[TOOL_CALLS]".len()..];
        let Some(args_marker) = after.find("[ARGS]") else {
            break;
        };
        let name = after[..args_marker].trim().to_string();
        let args_start = args_marker + "[ARGS]".len();
        let args_and_rest = &after[args_start..];
        let next = args_and_rest
            .find("[TOOL_CALLS]")
            .unwrap_or(args_and_rest.len());
        let raw_args = args_and_rest[..next].trim();
        if !name.is_empty() {
            let arguments = serde_json::from_str::<Value>(raw_args)
                .map(|v| json_arguments_to_string(&v))
                .unwrap_or_else(|_| raw_args.to_string());
            calls.push(ToolCall {
                id: stable_tool_call_id(&name, raw_args),
                call_type: "function".to_string(),
                function: FunctionCall { name, arguments },
            });
        }
        rest = &args_and_rest[next..];
    }
    calls
}

fn extract_lfm2_tool_calls(text: &str) -> Vec<ToolCall> {
    extract_tag_bodies(text, "<|tool_call_start|>", "<|tool_call_end|>")
        .into_iter()
        .flat_map(|body| {
            let body = body.trim();
            let body = body.strip_prefix('[').unwrap_or(body);
            let body = body.strip_suffix(']').unwrap_or(body);
            split_top_level(body, ',')
                .into_iter()
                .filter_map(parse_lfm2_call)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn parse_lfm2_call(raw: &str) -> Option<ToolCall> {
    let raw = raw.trim();
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    let name = raw[..open].trim().to_string();
    let arg_body = &raw[open + 1..close];
    let mut arguments = Map::new();
    for segment in split_top_level(arg_body, ',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((key, value)) = split_first_top_level(segment, '=') else {
            continue;
        };
        arguments.insert(key.trim().to_string(), parse_pythonish_value(value.trim()));
    }
    Some(ToolCall {
        id: stable_tool_call_id(&name, raw),
        call_type: "function".to_string(),
        function: FunctionCall {
            name,
            arguments: Value::Object(arguments).to_string(),
        },
    })
}

fn extract_function_gemma_tool_calls(text: &str) -> Vec<ToolCall> {
    extract_tag_bodies(text, "<start_function_call>", "<end_function_call>")
        .into_iter()
        .filter_map(parse_gemma_call_body)
        .collect()
}

fn parse_gemma_call_body(body: &str) -> Option<ToolCall> {
    let body = body.trim();
    let rest = body.strip_prefix("call:")?;
    let open = rest.find('{')?;
    let close = rest.rfind('}')?;
    let name = rest[..open].trim().to_string();
    let args_body = &rest[open + 1..close];
    let mut arguments = Map::new();
    for segment in split_top_level(args_body, ',') {
        let Some((key, value)) = split_first_top_level(segment.trim(), ':') else {
            continue;
        };
        let value = value
            .trim()
            .strip_prefix("<escape>")
            .and_then(|v| v.strip_suffix("<escape>"))
            .unwrap_or_else(|| value.trim());
        arguments.insert(key.trim().to_string(), parse_pythonish_value(value));
    }
    Some(ToolCall {
        id: stable_tool_call_id(&name, body),
        call_type: "function".to_string(),
        function: FunctionCall {
            name,
            arguments: Value::Object(arguments).to_string(),
        },
    })
}

fn extract_gemma4_tool_calls(text: &str) -> Vec<ToolCall> {
    extract_tag_bodies(text, "<|tool_call>", "<tool_call|>")
        .into_iter()
        .filter_map(|body| {
            let rest = body.trim().strip_prefix("call:")?;
            let open = rest.find('{')?;
            let close = rest.rfind('}')?;
            let name = rest[..open].trim().to_string();
            let args = parse_gemma4_object(&rest[open..=close])?;
            Some(ToolCall {
                id: stable_tool_call_id(&name, body),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name,
                    arguments: args.to_string(),
                },
            })
        })
        .collect()
}

fn parse_function_tool_call(body: &str) -> Option<ToolCall> {
    let function_start = body.find("<function=")?;
    let name_start = function_start + "<function=".len();
    let name_end = body[name_start..].find('>')? + name_start;
    let name = body[name_start..name_end].trim().to_string();
    let after_name = &body[name_end + 1..];
    let function_end = after_name.find("</function>").unwrap_or(after_name.len());
    let function_body = &after_name[..function_end];

    let mut arguments = Map::new();
    let mut rest = function_body;
    while let Some(param_start) = rest.find("<parameter=") {
        let key_start = param_start + "<parameter=".len();
        let Some(key_end_rel) = rest[key_start..].find('>') else {
            break;
        };
        let key_end = key_start + key_end_rel;
        let key = rest[key_start..key_end].trim().to_string();
        let value_start = key_end + 1;
        let Some(value_end_rel) = rest[value_start..].find("</parameter>") else {
            break;
        };
        let value_end = value_start + value_end_rel;
        let raw_value = rest[value_start..value_end].trim_matches('\n').trim();
        let value = serde_json::from_str::<Value>(raw_value)
            .unwrap_or_else(|_| Value::String(raw_value.to_string()));
        arguments.insert(key, value);
        rest = &rest[value_end + "</parameter>".len()..];
    }

    Some(ToolCall {
        id: stable_tool_call_id(&name, body),
        call_type: "function".to_string(),
        function: FunctionCall {
            name,
            arguments: Value::Object(arguments).to_string(),
        },
    })
}

fn strip_tool_blocks(text: &str) -> String {
    let mut out = strip_tagged_blocks(text, "<tool_call>", "</tool_call>");
    out = strip_tagged_blocks(&out, "<|tool_call>", "<tool_call|>");
    out = strip_tagged_blocks(&out, "<start_function_call>", "<end_function_call>");
    out = strip_tagged_blocks(&out, "<|tool_call_start|>", "<|tool_call_end|>");
    strip_ministral_blocks(&out)
}

fn strip_tagged_blocks(text: &str, start_tag: &str, end_tag: &str) -> String {
    let mut rest = text;
    let mut output = String::with_capacity(text.len());
    while let Some(start) = rest.find(start_tag) {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + start_tag.len()..];
        if let Some(end) = after_start.find(end_tag) {
            rest = &after_start[end + end_tag.len()..];
        } else {
            rest = "";
            break;
        }
    }
    output.push_str(rest);
    output
}

fn strip_ministral_blocks(text: &str) -> String {
    match text.find("[TOOL_CALLS]") {
        Some(start) => text[..start].to_string(),
        None => text.to_string(),
    }
}

fn extract_tag_bodies<'a>(text: &'a str, start_tag: &str, end_tag: &str) -> Vec<&'a str> {
    let mut bodies = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(start_tag) {
        let after_start = &rest[start + start_tag.len()..];
        let Some(end) = after_start.find(end_tag) else {
            break;
        };
        bodies.push(&after_start[..end]);
        rest = &after_start[end + end_tag.len()..];
    }
    bodies
}

fn split_top_level(s: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0usize;

    for (idx, ch) in s.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            c if c == delimiter && depth == 0 => {
                parts.push(&s[start..idx]);
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

fn split_first_top_level(s: &str, delimiter: char) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in s.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            c if c == delimiter && depth == 0 => {
                return Some((&s[..idx], &s[idx + c.len_utf8()..]));
            }
            _ => {}
        }
    }
    None
}

fn parse_pythonish_value(raw: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        return value;
    }
    match raw {
        "True" => Value::Bool(true),
        "False" => Value::Bool(false),
        "None" => Value::Null,
        _ if raw.len() >= 2 && raw.starts_with('\'') && raw.ends_with('\'') => {
            Value::String(raw[1..raw.len() - 1].to_string())
        }
        _ => Value::String(raw.to_string()),
    }
}

fn parse_gemma4_object(raw: &str) -> Option<Value> {
    let raw = raw.trim();
    let inner = raw.strip_prefix('{')?.strip_suffix('}')?;
    let mut map = Map::new();
    for segment in split_top_level(inner, ',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((key, value)) = split_first_top_level(segment, ':') else {
            continue;
        };
        map.insert(key.trim().to_string(), parse_gemma4_value(value.trim()));
    }
    Some(Value::Object(map))
}

fn parse_gemma4_value(raw: &str) -> Value {
    if let Some(value) = raw
        .strip_prefix("<|\"|>")
        .and_then(|v| v.strip_suffix("<|\"|>"))
    {
        return Value::String(value.to_string());
    }
    if raw.starts_with('{') && raw.ends_with('}') {
        return parse_gemma4_object(raw).unwrap_or_else(|| Value::String(raw.to_string()));
    }
    if raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        return Value::Array(
            split_top_level(inner, ',')
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .map(|s| parse_gemma4_value(s.trim()))
                .collect(),
        );
    }
    parse_pythonish_value(raw)
}

fn json_arguments_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn stable_tool_call_id(name: &str, raw: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in name.bytes().chain(raw.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("call_{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tool() -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: "glob".to_string(),
                description: "List files".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "limit": {"type": "integer"},
                        "required_only": {"type": "boolean"}
                    },
                    "required": ["pattern"]
                }),
            },
        }
    }

    #[test]
    fn falls_back_to_model_hints() {
        assert_eq!(
            ToolFormat::detect("", Some("qwen3.5"), None),
            Some(ToolFormat::QwenFunction)
        );
        assert_eq!(
            ToolFormat::detect("", None, Some("gemma-4")),
            Some(ToolFormat::Gemma4)
        );
    }

    #[test]
    fn builds_qwen3_tool_grammar() {
        let tool = sample_tool();
        let grammar = ToolFormat::Qwen3.grammar(&[tool.clone()]).unwrap();
        assert!(grammar.lazy);
        assert!(grammar.grammar.contains("<tool_call>"));
        assert!(grammar.grammar.contains("\"glob\""));
        assert_eq!(grammar.triggers[0].value, "<tool_call>");
        assert!(!grammar.grammar.contains(r"\-"));

        for (format, trigger) in [
            (ToolFormat::QwenFunction, "<tool_call>"),
            (ToolFormat::Ministral3, "[TOOL_CALLS]"),
            (ToolFormat::Lfm2, "<|tool_call_start|>"),
            (ToolFormat::FunctionGemma, "<start_function_call>"),
            (ToolFormat::Gemma4, "<|tool_call>"),
        ] {
            let grammar = format.grammar(&[tool.clone()]).unwrap();
            assert!(grammar.lazy);
            assert_eq!(grammar.triggers[0].value, trigger);
        }
    }

    #[test]
    fn qwen_function_grammar_is_schema_aware() {
        let grammar = ToolFormat::QwenFunction.grammar(&[sample_tool()]).unwrap();
        assert!(grammar.grammar.contains("<function=glob>\\n"));
        assert!(grammar.grammar.contains("<parameter=pattern>\\n"));
        assert!(grammar.grammar.contains("<parameter=limit>\\n"));
        assert!(grammar.grammar.contains("\"true\" | \"false\""));
    }

    #[test]
    fn parses_qwen_json_tool_call() {
        let parsed = parse_assistant_format(
            r#"<think>checking</think><tool_call>{"name":"glob","arguments":{"pattern":"**/*.rs"}}</tool_call>"#,
        );
        assert_eq!(parsed.thinking.as_deref(), Some("checking"));
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls[0].function.name, "glob");
        assert_eq!(calls[0].function.arguments, r#"{"pattern":"**/*.rs"}"#);
    }

    #[test]
    fn parses_qwen_function_tool_call() {
        let parsed = parse_assistant_format(
            "<tool_call>\n<function=get_weather>\n<parameter=city>\nCopenhagen\n</parameter>\n<parameter=days>\n3\n</parameter>\n</function>\n</tool_call>",
        );
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls[0].function.name, "get_weather");
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["city"], "Copenhagen");
        assert_eq!(args["days"], 3);
    }

    #[test]
    fn parses_when_generation_starts_inside_thinking() {
        let parsed = parse_assistant_format_with_state(
            "Thinking Process:\n1. test\n</think><tool_call>{\"name\":\"glob\",\"arguments\":{\"pattern\":\"**/*.rs\"}}</tool_call>",
            ReasoningFormat::ThinkTags,
            true,
        );
        assert_eq!(
            parsed.thinking.as_deref(),
            Some("Thinking Process:\n1. test")
        );
        assert!(parsed.content.is_empty());
        assert_eq!(parsed.tool_calls.unwrap()[0].function.name, "glob");
    }

    #[test]
    fn parses_gemma_channel_reasoning() {
        let parsed = parse_assistant_format_with_state(
            "<|channel>thought\ncheck system<channel|>All good",
            ReasoningFormat::GemmaChannel {
                implicit_leading_reasoning_prefix: false,
            },
            false,
        );
        assert_eq!(parsed.thinking.as_deref(), Some("check system"));
        assert_eq!(parsed.content, "All good");
    }

    #[test]
    fn parses_implicit_gemma_channel_reasoning() {
        let parsed = parse_assistant_format_with_state(
            "thought\nconsider options<channel|>Final",
            ReasoningFormat::GemmaChannel {
                implicit_leading_reasoning_prefix: true,
            },
            true,
        );
        assert_eq!(parsed.thinking.as_deref(), Some("consider options"));
        assert_eq!(parsed.content, "Final");
    }

    #[test]
    fn parses_gemma4_tool_call_with_nested_values() {
        let parsed = parse_assistant_format_with_state(
            "<|tool_call>call:create_event{name:<|\"|>Meeting<|\"|>,location:{city:<|\"|>NYC<|\"|>,floor:3},tags:[<|\"|>work<|\"|>,<|\"|>urgent<|\"|>],rating:4.5}<tool_call|>",
            ReasoningFormat::GemmaChannel {
                implicit_leading_reasoning_prefix: false,
            },
            false,
        );
        let calls = parsed.tool_calls.unwrap();
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(calls[0].function.name, "create_event");
        assert_eq!(args["name"], "Meeting");
        assert_eq!(args["location"]["city"], "NYC");
        assert_eq!(args["location"]["floor"], 3);
        assert_eq!(args["tags"][0], "work");
        assert_eq!(args["tags"][1], "urgent");
        assert_eq!(args["rating"], 4.5);
    }
}
