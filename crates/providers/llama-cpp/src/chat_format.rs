use crate::common_chat::{GrammarTrigger, ReasoningFormat, ToolGrammar};
use crate::gbnf::{ExclusionDfa, gbnf_literal};
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
    parse_assistant_format_with_tools(text, reasoning_format, starts_in_thinking, &[])
}

/// Like [`parse_assistant_format_with_state`], but uses the tool schemas so
/// that string parameters are returned exactly as the model wrote them.
pub(crate) fn parse_assistant_format_with_tools(
    text: &str,
    reasoning_format: ReasoningFormat,
    starts_in_thinking: bool,
    tools: &[Tool],
) -> ParsedFormat {
    let (thinking, without_thinking) =
        extract_reasoning_blocks(text, reasoning_format, starts_in_thinking);
    let (calls, remaining) = extract_tool_calls(&without_thinking, tools);
    let content = strip_tool_blocks(&remaining).trim().to_string();

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

/// Returns the parsed calls and the text with Qwen function-call blocks
/// removed. Those blocks are located structurally, so tool arguments that
/// themselves contain markers such as `</tool_call>` or `[TOOL_CALLS]` neither
/// end the call early nor leak into the other extractors.
fn extract_tool_calls(text: &str, tools: &[Tool]) -> (Option<Vec<ToolCall>>, String) {
    let spans = scan_qwen_function_calls(text, tools);
    let mut remaining = String::with_capacity(text.len());
    let mut qwen_calls = Vec::new();
    let mut cursor = 0;
    for span in spans {
        remaining.push_str(&text[cursor..span.start]);
        cursor = span.end;
        qwen_calls.extend(span.call);
    }
    remaining.push_str(&text[cursor..]);

    let mut calls = Vec::new();
    calls.extend(extract_qwen_json_tool_calls(&remaining));
    calls.extend(qwen_calls);
    calls.extend(extract_ministral_tool_calls(&remaining));
    calls.extend(extract_lfm2_tool_calls(&remaining));
    calls.extend(extract_function_gemma_tool_calls(&remaining, tools));
    calls.extend(extract_gemma4_tool_calls(&remaining));
    ((!calls.is_empty()).then_some(calls), remaining)
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
ws ::= [ \t\n\r]*
{value_text_rules}
{tool_specific_rules}"#,
            tool_rules = tool_rules.root_alternation,
            // String values may contain anything (including `<`) except the
            // exact closing tag that follows them.
            value_text_rules = ExclusionDfa::new("</parameter>").rules("value-text"),
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
    // Escaped values may contain anything except the closing `<escape>`.
    let value_text_rules = ExclusionDfa::new("<escape>").rules("value-text");
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= gemma-fn-call+
gemma-fn-call ::= "<start_function_call>" "call:" tool-name "{{" ws (gemma-param ("," ws gemma-param)*)? ws "}}" "<end_function_call>" ws
gemma-param ::= identifier ws ":" ws "<escape>" value-text "<escape>"
identifier ::= [A-Za-z_][A-Za-z0-9_-]*
tool-name ::= {names}
ws ::= [ \t\n\r]*
{value_text_rules}"#
        ),
        root: "root",
        lazy: true,
        triggers: word_trigger("<start_function_call>"),
    })
}

fn gemma4_tool_grammar(tools: &[Tool]) -> Option<ToolGrammar> {
    let names = tool_names_rule(tools)?;
    // Strings are delimited by `<|"|>` and carry raw text: no JSON escaping,
    // and a plain `"` is fine. Only the closing delimiter is excluded.
    let text_rules = ExclusionDfa::new("<|\"|>").rules("gemma4-text");
    Some(ToolGrammar {
        grammar: format!(
            r#"root ::= gemma4-call+
gemma4-call ::= "<|tool_call>" "call:" tool-name gemma4-object "<tool_call|>" ws
gemma4-object ::= "{{" ws (gemma4-member ("," ws gemma4-member)*)? ws "}}"
gemma4-member ::= identifier ws ":" ws gemma4-value
gemma4-array ::= "[" ws (gemma4-value ("," ws gemma4-value)*)? ws "]"
gemma4-value ::= gemma4-object | gemma4-array | gemma4-string | number | "true" | "false" | "null"
gemma4-string ::= "<|\"|>" gemma4-text "<|\"|>"
identifier ::= [A-Za-z_][A-Za-z0-9_-]*
tool-name ::= {names}
number ::= "-"? ([0-9] | [1-9] [0-9]*) ("." [0-9]+)? ([eE] [+-]? [0-9]+)?
ws ::= [ \t\n\r]*
{text_rules}"#,
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

/// Declared JSON-schema types, whether `type` is a single string or an array.
fn schema_types(schema: &Value) -> impl Iterator<Item = &str> {
    let ty = schema.get("type");
    ty.and_then(Value::as_str).into_iter().chain(
        ty.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    )
}

/// True when the schema allows a string, i.e. the value must reach the tool
/// verbatim instead of being coerced through JSON.
fn is_text_param(schema: &Value) -> bool {
    schema_types(schema).any(|t| t == "string")
}

/// Value for a text parameter: the raw text, except a literal `null` when
/// the schema explicitly allows `"null"`.
fn text_param_value(raw: &str, schema: &Value) -> Value {
    if raw == "null" && schema_types(schema).any(|t| t == "null") {
        Value::Null
    } else {
        Value::String(raw.to_string())
    }
}

/// Look up the JSON schema of parameter `param` of tool `tool`, if declared.
fn param_schema<'a>(tools: &'a [Tool], tool: &str, param: &str) -> Option<&'a Value> {
    tools
        .iter()
        .find(|t| t.function.name == tool)?
        .function
        .parameters
        .get("properties")?
        .get(param)
}

/// Convert the raw text of a parameter into a JSON value. Declared strings
/// stay exactly as written; everything else is parsed as JSON when possible.
fn coerce_param_value(raw: &str, schema: Option<&Value>) -> Value {
    if let Some(schema) = schema.filter(|s| is_text_param(s)) {
        return text_param_value(raw, schema);
    }
    let trimmed = raw.trim();
    serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(trimmed.to_string()))
}

/// GBNF expression matching a parameter value of the given schema type.
fn parameter_value_expr(schema: &Value, _rule_name: &str) -> String {
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
        // Unknown type: accept any text rather than emitting a rule that
        // refers to itself (which llama.cpp rejects for the whole request).
        Some(_) => "value-text".to_string(),
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

/// A `<tool_call><function=..>` block located in the text. `call` is `None`
/// when the block is incomplete (e.g. generation hit the token limit); the
/// span then runs to the end of the text so it is dropped from the content.
struct QwenSpan {
    start: usize,
    end: usize,
    call: Option<ToolCall>,
}

/// Return the first offset at or after `pos` that is not whitespace.
fn skip_ws(text: &str, pos: usize) -> usize {
    let rest = &text[pos..];
    pos + (rest.len() - rest.trim_start().len())
}

/// Find every `<tool_call><function=..>` block and the span it occupies.
fn scan_qwen_function_calls(text: &str, tools: &[Tool]) -> Vec<QwenSpan> {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(rel) = text[pos..].find(OPEN) {
        let start = pos + rel;
        let body_start = start + OPEN.len();
        let fn_start = skip_ws(text, body_start);
        if !text[fn_start..].starts_with("<function=") {
            // JSON-style call (or something else); not ours.
            pos = body_start;
            continue;
        }
        let parsed = parse_function_block(text, fn_start, tools).and_then(|(call, fn_end)| {
            let close_at = skip_ws(text, fn_end);
            text[close_at..]
                .starts_with(CLOSE)
                .then_some((call, close_at + CLOSE.len()))
        });
        match parsed {
            Some((call, end)) => {
                spans.push(QwenSpan {
                    start,
                    end,
                    call: Some(call),
                });
                pos = end;
            }
            None => {
                spans.push(QwenSpan {
                    start,
                    end: text.len(),
                    call: None,
                });
                break;
            }
        }
    }
    spans
}

/// Remove the single newline the chat template puts on each side of a value.
fn strip_value_padding(raw: &str) -> &str {
    let raw = raw
        .strip_prefix("\r\n")
        .or_else(|| raw.strip_prefix('\n'))
        .unwrap_or(raw);
    raw.strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw)
}

/// Parse `<function=NAME> <parameter=K>\nV\n</parameter>.. </function>`
/// starting at `fn_start`. Returns the call and the offset just past the
/// block. Parameters are consumed sequentially, so a value is never searched
/// for markers like `</function>`.
fn parse_function_block(text: &str, fn_start: usize, tools: &[Tool]) -> Option<(ToolCall, usize)> {
    let name_start = fn_start + "<function=".len();
    let name_end = text[name_start..].find('>')? + name_start;
    let name = text[name_start..name_end].trim().to_string();
    let mut cursor = name_end + 1;
    let mut arguments = Map::new();
    loop {
        let at = skip_ws(text, cursor);
        if text[at..].starts_with("<parameter=") {
            let key_start = at + "<parameter=".len();
            let key_end = text[key_start..].find('>')? + key_start;
            let key = text[key_start..key_end].trim().to_string();
            let value_start = key_end + 1;
            let value_end = text[value_start..].find("</parameter>")? + value_start;
            let raw = strip_value_padding(&text[value_start..value_end]);
            let value = coerce_param_value(raw, param_schema(tools, &name, &key));
            arguments.insert(key, value);
            cursor = value_end + "</parameter>".len();
        } else if text[at..].starts_with("</function>") {
            cursor = at + "</function>".len();
            break;
        } else {
            // Tolerate a missing `</function>`; the caller checks `</tool_call>`.
            break;
        }
    }
    Some((
        ToolCall {
            id: stable_tool_call_id(&name, &text[fn_start..cursor]),
            call_type: "function".to_string(),
            function: FunctionCall {
                name,
                arguments: Value::Object(arguments).to_string(),
            },
        },
        cursor,
    ))
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

/// Extract complete `<start_function_call>..<end_function_call>` calls.
fn extract_function_gemma_tool_calls(text: &str, tools: &[Tool]) -> Vec<ToolCall> {
    const OPEN: &str = "<start_function_call>";
    const CLOSE: &str = "<end_function_call>";
    let mut calls = Vec::new();
    let mut pos = 0;
    while let Some(rel) = text[pos..].find(OPEN) {
        let body_start = pos + rel + OPEN.len();
        let Some((call, end)) = parse_gemma_call_body(text, body_start, tools) else {
            pos = body_start;
            continue;
        };
        let close_at = skip_ws(text, end);
        if !text[close_at..].starts_with(CLOSE) {
            pos = body_start;
            continue;
        }
        calls.push(call);
        pos = close_at + CLOSE.len();
    }
    calls
}

/// Parse `call:NAME{key:<escape>VALUE<escape>,..}` starting at `start`.
/// Escaped values are consumed up to the next `<escape>`, so commas, braces
/// and colons inside them are preserved. Returns the call and the offset just
/// past the closing `}`.
fn parse_gemma_call_body(text: &str, start: usize, tools: &[Tool]) -> Option<(ToolCall, usize)> {
    const ESC: &str = "<escape>";
    let at = skip_ws(text, start);
    let after_call = text[at..].strip_prefix("call:")?;
    let name_start = text.len() - after_call.len();
    let open = text[name_start..].find('{')? + name_start;
    let name = text[name_start..open].trim().to_string();
    let mut cursor = open + 1;
    let mut arguments = Map::new();
    loop {
        cursor = skip_ws(text, cursor);
        if text[cursor..].starts_with(',') {
            cursor += 1;
            continue;
        }
        if text[cursor..].starts_with('}') {
            cursor += 1;
            break;
        }
        let colon = text[cursor..].find(':')? + cursor;
        let key = text[cursor..colon].trim().to_string();
        cursor = skip_ws(text, colon + 1);
        let schema = param_schema(tools, &name, &key);
        let value = if text[cursor..].starts_with(ESC) {
            let value_start = cursor + ESC.len();
            let value_end = text[value_start..].find(ESC)? + value_start;
            cursor = value_end + ESC.len();
            let raw = &text[value_start..value_end];
            if let Some(schema) = schema.filter(|s| is_text_param(s)) {
                text_param_value(raw, schema)
            } else {
                parse_pythonish_value(raw.trim())
            }
        } else {
            let end = text[cursor..]
                .find([',', '}'])
                .map_or(text.len(), |i| i + cursor);
            let raw = text[cursor..end].trim();
            cursor = end;
            parse_pythonish_value(raw)
        };
        arguments.insert(key, value);
    }
    Some((
        ToolCall {
            id: stable_tool_call_id(&name, &text[at..cursor]),
            call_type: "function".to_string(),
            function: FunctionCall {
                name,
                arguments: Value::Object(arguments).to_string(),
            },
        },
        cursor,
    ))
}

fn extract_gemma4_tool_calls(text: &str) -> Vec<ToolCall> {
    const OPEN: &str = "<|tool_call>";
    const CLOSE: &str = "<tool_call|>";
    let mut calls = Vec::new();
    let mut pos = 0;
    while let Some(rel) = text[pos..].find(OPEN) {
        let body_start = pos + rel + OPEN.len();
        pos = body_start;
        let at = skip_ws(text, body_start);
        let Some(rest) = text[at..].strip_prefix("call:") else {
            continue;
        };
        let name_start = text.len() - rest.len();
        let Some(open) = text[name_start..].find('{').map(|i| i + name_start) else {
            continue;
        };
        let name = text[name_start..open].trim().to_string();
        let mut cursor = open;
        let Some(args) = parse_gemma4_object_at(text, &mut cursor) else {
            continue;
        };
        let close_at = skip_ws(text, cursor);
        if !text[close_at..].starts_with(CLOSE) {
            continue;
        }
        calls.push(ToolCall {
            id: stable_tool_call_id(&name, &text[at..cursor]),
            call_type: "function".to_string(),
            function: FunctionCall {
                name,
                arguments: args.to_string(),
            },
        });
        pos = close_at + CLOSE.len();
    }
    calls
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

/// Parse a Gemma 4 object starting at `*pos` (which must point at `{`).
/// Strings are `<|"|>raw text<|"|>` and are read up to the closing
/// delimiter, so commas, braces and quotes inside them are preserved.
fn parse_gemma4_object_at(text: &str, pos: &mut usize) -> Option<Value> {
    if !text[*pos..].starts_with('{') {
        return None;
    }
    *pos += 1;
    let mut map = Map::new();
    loop {
        *pos = skip_ws(text, *pos);
        if text[*pos..].starts_with(',') {
            *pos += 1;
        } else if text[*pos..].starts_with('}') {
            *pos += 1;
            return Some(Value::Object(map));
        } else {
            let colon = text[*pos..].find(':')? + *pos;
            let key = text[*pos..colon].trim().trim_matches('"').to_string();
            *pos = colon + 1;
            map.insert(key, parse_gemma4_value_at(text, pos)?);
        }
    }
}

/// Parse one Gemma 4 argument value at `pos`, advancing `pos` past it.
fn parse_gemma4_value_at(text: &str, pos: &mut usize) -> Option<Value> {
    const Q: &str = "<|\"|>";
    *pos = skip_ws(text, *pos);
    let rest = &text[*pos..];
    if rest.starts_with(Q) {
        let start = *pos + Q.len();
        let end = text[start..].find(Q)? + start;
        *pos = end + Q.len();
        return Some(Value::String(text[start..end].to_string()));
    }
    if rest.starts_with('{') {
        return parse_gemma4_object_at(text, pos);
    }
    if rest.starts_with('[') {
        *pos += 1;
        let mut items = Vec::new();
        loop {
            *pos = skip_ws(text, *pos);
            if text[*pos..].starts_with(',') {
                *pos += 1;
            } else if text[*pos..].starts_with(']') {
                *pos += 1;
                return Some(Value::Array(items));
            } else {
                items.push(parse_gemma4_value_at(text, pos)?);
            }
        }
    }
    let end = rest.find([',', '}', ']']).map_or(text.len(), |i| i + *pos);
    let raw = text[*pos..end].trim();
    *pos = end;
    (!raw.is_empty()).then(|| parse_pythonish_value(raw))
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
                strict: None,
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

    /// `write_file` tool with string, integer and unknown-typed parameters.
    fn write_file_tool() -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: "write_file".to_string(),
                description: "Write a file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"},
                        "mode": {"type": "integer"},
                        "weird": {"type": "frobnicate"}
                    },
                    "required": ["path", "content"]
                }),
                strict: None,
            },
        }
    }

    /// Parse `text` against [`write_file_tool`] and return its tool calls.
    fn parse_with_tools(text: &str) -> Vec<ToolCall> {
        parse_assistant_format_with_tools(
            text,
            ReasoningFormat::ThinkTags,
            false,
            &[write_file_tool()],
        )
        .tool_calls
        .unwrap_or_default()
    }

    /// Render a Qwen `<function=write_file>` call with the given content.
    fn qwen_call(content: &str) -> String {
        format!(
            "<tool_call>\n<function=write_file>\n<parameter=path>\na.py\n</parameter>\n<parameter=content>\n{content}\n</parameter>\n</function>\n</tool_call>"
        )
    }

    #[test]
    fn qwen_and_gemma_grammars_allow_lt_in_string_values() {
        let tool = write_file_tool();
        let qwen = ToolFormat::QwenFunction
            .grammar(std::slice::from_ref(&tool))
            .unwrap();
        assert!(!qwen.grammar.contains("[^<]*"), "{}", qwen.grammar);
        assert!(qwen.grammar.contains("value-text-0 ::="));
        let gemma = ToolFormat::FunctionGemma.grammar(&[tool]).unwrap();
        assert!(!gemma.grammar.contains("[^<]*"), "{}", gemma.grammar);
        assert!(gemma.grammar.contains("value-text-0 ::="));
    }

    #[test]
    fn unknown_parameter_type_does_not_produce_self_referential_rule() {
        let grammar = ToolFormat::QwenFunction
            .grammar(&[write_file_tool()])
            .unwrap()
            .grammar;
        assert!(
            grammar.contains("qwen-fn-0-weird-value ::= value-text\n"),
            "{grammar}"
        );
    }

    #[test]
    fn gemma4_grammar_allows_plain_quotes_in_strings() {
        let grammar = ToolFormat::Gemma4
            .grammar(&[write_file_tool()])
            .unwrap()
            .grammar;
        assert!(grammar.contains("gemma4-text-0 ::="), "{grammar}");
        assert!(!grammar.contains("characters"), "{grammar}");
    }

    #[test]
    fn qwen_function_keeps_string_values_verbatim() {
        let content = "def f(row, best_d):\n    if row[j] < best_d:\n        return x <= y\n";
        let calls = parse_with_tools(&qwen_call(content));
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["path"], "a.py");
        assert_eq!(args["content"], content);
    }

    #[test]
    fn qwen_function_does_not_coerce_string_params_to_json() {
        for literal in ["123", "true", "null", r#"{"a": 1}"#, r#""quoted""#] {
            let calls = parse_with_tools(&qwen_call(literal));
            let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
            assert_eq!(args["content"], literal, "literal {literal:?}");
        }
    }

    #[test]
    fn qwen_function_preserves_indentation_and_trailing_newline() {
        // The template adds exactly one `\n` on each side; the rest is content.
        let calls = parse_with_tools(&qwen_call("    indented\n\n"));
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["content"], "    indented\n\n");
    }

    #[test]
    fn qwen_function_value_may_contain_markers_and_html() {
        let content = "<div>a</div>\n</function>\n</tool_call>\n[TOOL_CALLS]x[ARGS]{}";
        let parsed = parse_assistant_format_with_tools(
            &qwen_call(content),
            ReasoningFormat::ThinkTags,
            false,
            &[write_file_tool()],
        );
        let calls = parsed.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["content"], content);
        assert!(parsed.content.is_empty(), "{:?}", parsed.content);
    }

    #[test]
    fn qwen_function_still_coerces_typed_params() {
        let text = "<tool_call>\n<function=write_file>\n<parameter=path>\na\n</parameter>\n<parameter=content>\nx\n</parameter>\n<parameter=mode>\n420\n</parameter>\n</function>\n</tool_call>";
        let calls = parse_with_tools(text);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["mode"], 420);
    }

    #[test]
    fn qwen_function_incomplete_call_is_dropped_from_content() {
        let text = "Sure.\n<tool_call>\n<function=write_file>\n<parameter=content>\nif a <";
        let parsed = parse_assistant_format_with_tools(
            text,
            ReasoningFormat::ThinkTags,
            false,
            &[write_file_tool()],
        );
        assert!(parsed.tool_calls.is_none());
        assert_eq!(parsed.content, "Sure.");
    }

    #[test]
    fn type_arrays_containing_string_stay_verbatim() {
        let nullable = serde_json::json!({"type": ["string", "null"]});
        assert_eq!(coerce_param_value("42", Some(&nullable)), "42");
        assert_eq!(coerce_param_value(" true \n", Some(&nullable)), " true \n");
        assert_eq!(coerce_param_value("null", Some(&nullable)), Value::Null);
        let plain = serde_json::json!({"type": "string"});
        assert_eq!(coerce_param_value("null", Some(&plain)), "null");
        let untyped = serde_json::json!({});
        assert_eq!(coerce_param_value("null", Some(&untyped)), Value::Null);
        assert_eq!(coerce_param_value("42", Some(&untyped)), 42);
    }

    #[test]
    fn function_gemma_keeps_nullable_string_verbatim() {
        let tool = Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: "note".to_string(),
                description: "Take a note".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"text": {"type": ["string", "null"]}}
                }),
                strict: None,
            },
        };
        let parse = |value: &str| -> Value {
            let text = format!(
                "<start_function_call>call:note{{text:<escape>{value}<escape>}}<end_function_call>"
            );
            let calls = parse_assistant_format_with_tools(
                &text,
                ReasoningFormat::ThinkTags,
                false,
                std::slice::from_ref(&tool),
            )
            .tool_calls
            .unwrap();
            serde_json::from_str::<Value>(&calls[0].function.arguments).unwrap()["text"].clone()
        };
        assert_eq!(parse(" 123 "), " 123 ");
        assert_eq!(parse("null"), Value::Null);
    }

    #[test]
    fn function_gemma_values_keep_commas_braces_and_lt() {
        let text = "<start_function_call>call:write_file{path:<escape>a.py<escape>,content:<escape>if a < b, c: {d}<escape>}<end_function_call>";
        let calls = parse_with_tools(text);
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["path"], "a.py");
        assert_eq!(args["content"], "if a < b, c: {d}");
    }

    #[test]
    fn gemma4_strings_keep_quotes_commas_and_braces() {
        let text = "<|tool_call>call:write_file{path:<|\"|>a.py<|\"|>,content:<|\"|>print(\"hi, {x}\")<|\"|>,mode:420}<tool_call|>";
        let parsed = parse_assistant_format_with_state(
            text,
            ReasoningFormat::GemmaChannel {
                implicit_leading_reasoning_prefix: false,
            },
            false,
        );
        let calls = parsed.tool_calls.unwrap();
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["content"], "print(\"hi, {x}\")");
        assert_eq!(args["mode"], 420);
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
