//! Lark grammars for what follows each tool-call trigger.
//!
//! One generator per tool-call format, each in two tiers (design D1):
//!
//! * [`Tier::Schema`] restricts every call to a declared tool and its
//!   arguments to that tool's JSON schema.
//! * [`Tier::Generic`] checks call structure and tool names only and accepts
//!   the same language as the GBNF grammar of the format.
//!
//! The trigger itself (`<tool_call>`, `[TOOL_CALLS]`, ...) is consumed by
//! [`crate::tools::llg::LlgToolGrammar`] before the matcher starts, so every
//! grammar here begins right after it. Delimiters go through a [`Tag`]
//! closure that knows which of them are single special tokens in the
//! model's vocabulary (design D3).

use crate::chat_format::required_params;
use querymt::chat::Tool;
use serde_json::Value;
use std::fmt::Write as _;

/// How closely a grammar follows the tools' JSON schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Parameter names, types, enums and required parameters follow the
    /// schema; array and object values follow their own schema.
    Schema,
    /// Call structure and tool names only.
    Generic,
}

/// A format delimiter as a Lark expression.
pub(crate) struct Delim {
    expr: String,
    /// One special token in the vocabulary. llguidance spells those as
    /// `0xFF`-prefixed bytes, which a text lexeme can never consume.
    special: bool,
}

impl Delim {
    /// Delimiter spelled out as text.
    pub(crate) fn literal(text: &str) -> Self {
        Self {
            expr: lark_string(text),
            special: false,
        }
    }

    /// Delimiter that is the single special token `id` (or its spelling).
    pub(crate) fn special(id: u32, text: &str) -> Self {
        Self {
            expr: format!("(<[{id}]> | {})", lark_string(text)),
            special: true,
        }
    }
}

/// Maps a delimiter's text to its Lark expression.
pub(crate) type Tag<'a> = &'a dyn Fn(&str) -> Delim;

/// Lark string literal (JSON string syntax).
pub(crate) fn lark_string(text: &str) -> String {
    serde_json::to_string(text).expect("string serializes")
}

/// Escape `text` for use inside a Lark `/regex/`.
fn lark_regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
            | '/' | '-' | '&' | '~' | '#' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

/// Terminal matching any text that does not contain `terminator`; the
/// same language as [`crate::gbnf::ExclusionDfa`], as one regex lexeme.
fn exclusion_terminal(name: &str, terminator: &str) -> String {
    format!(
        "{name}: /(?s:.*)/ & ~/(?s:.*){}(?s:.*)/\n",
        lark_regex_escape(terminator)
    )
}

/// Terminals every grammar may use.
const TERMINALS: &str = r"WS: /[ \t\n\r]+/
NUMBER: /-?([0-9]|[1-9][0-9]*)(\.[0-9]+)?([eE][+\-]?[0-9]+)?/
IDENT: /[A-Za-z_][A-Za-z0-9_\-]*/
";

/// JSON rules for the generic tier, the same language as `json_gbnf_rules`
/// in `chat_format`. They are not `%json`: that follows the JSON spec and
/// forbids raw control characters in strings, which the GBNF rule
/// `[^"\\]` accepts.
const GENERIC_JSON: &str = r#"JSTRING: /"([^"\\]|\\(["\\\/bfnrt]|u[0-9a-fA-F]{4}))*"/
jvalue: jobject | jarray | JSTRING | NUMBER | "true" | "false" | "null"
jobject: "{" WS? (jmember (WS? "," WS? jmember)*)? WS? "}"
jmember: JSTRING WS? ":" WS? jvalue
jarray: "[" WS? (jvalue (WS? "," WS? jvalue)*)? WS? "]"
"#;

/// Terminals, plus the GBNF-equivalent JSON rules for the generic tier.
fn push_terminals(out: &mut String, tier: Tier, json: bool) {
    out.push_str(TERMINALS);
    if json && tier == Tier::Generic {
        out.push_str(GENERIC_JSON);
    }
}

/// Rule `name` for text that ends with `close`, containing `exclude` nowhere.
///
/// A special-token terminator is a separate item after the text lexeme. An
/// ordinary-text one is attached to the lexeme as its `suffix`, which makes
/// the lexeme end at the first occurrence (design D3).
fn text_rule(out: &mut String, name: &str, exclude: &str, close: &Delim, suffix: &str) {
    let terminal = name.to_ascii_uppercase();
    if close.special {
        let _ = writeln!(out, "{name}: {terminal} {}", close.expr);
    } else {
        let _ = writeln!(out, "{name}[suffix={}]: {terminal}", lark_string(suffix));
    }
    out.push_str(&exclusion_terminal(&terminal, exclude));
}

/// `%json` expression for `schema`, compiled leniently: `oneOf` is treated
/// as `anyOf` and unsupported keywords are ignored. The options travel in
/// the schema itself (`x-guidance`), as `%json` has no option syntax.
pub(crate) fn json_schema_expr(schema: &Value) -> String {
    let mut schema = schema.clone();
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "x-guidance".into(),
            serde_json::json!({"coerce_one_of": true, "lenient": true}),
        );
    }
    format!("%json {schema}")
}

/// `%json` expression for one value: its schema, or any JSON.
fn json_value_expr(schema: &Value, tier: Tier) -> String {
    if tier == Tier::Schema && schema.is_object() {
        json_schema_expr(schema)
    } else {
        "%json {}".to_string()
    }
}

/// The arguments schema of `tool` as an object schema.
fn object_schema(tool: &Tool) -> Value {
    let mut schema = tool.function.parameters.clone();
    match schema.as_object_mut() {
        Some(object) => {
            object.entry("type").or_insert_with(|| "object".into());
        }
        None => schema = serde_json::json!({"type": "object"}),
    }
    schema
}

/// `"a" | "b"` over the tool names.
fn name_alternatives(tools: &[Tool]) -> String {
    tools
        .iter()
        .map(|tool| lark_string(&tool.function.name))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn alternatives(count: usize) -> String {
    (0..count)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// A parameter a tool declares.
struct Declared<'a> {
    name: &'a str,
    schema: &'a Value,
    required: bool,
}

/// Declared parameters in schema order, which is also the order models are
/// trained to write them in (design D2).
fn declared(schema: &Value) -> Vec<Declared<'_>> {
    let required = required_params(schema);
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .map(|(name, schema)| Declared {
                    name,
                    schema,
                    required: required.contains(name.as_str()),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// JSON Schema allows undeclared properties unless told otherwise.
fn extras_allowed(schema: &Value) -> bool {
    schema.get("additionalProperties") != Some(&Value::Bool(false))
}

/// What a parameter's schema asks for, independent of the format.
enum Kind<'a> {
    Text,
    Enum(Vec<&'a str>),
    Number,
    Bool,
    Null,
    Array,
    Object,
    /// No usable type.
    Any,
}

fn classify(schema: &Value) -> Kind<'_> {
    let ty = schema.get("type").and_then(Value::as_str);
    match ty {
        Some("string") | None => {
            let values: Vec<&str> = schema
                .get("enum")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if !values.is_empty() {
                Kind::Enum(values)
            } else if ty.is_some() {
                Kind::Text
            } else {
                Kind::Any
            }
        }
        Some("integer" | "number") => Kind::Number,
        Some("boolean") => Kind::Bool,
        Some("null") => Kind::Null,
        Some("array") => Kind::Array,
        Some("object") => Kind::Object,
        Some(_) => Kind::Any,
    }
}

/// `("a" | "b")` over `values`, each wrapped by `wrap`.
fn enum_expr<'a>(values: &[&'a str], wrap: impl Fn(&'a str) -> String) -> String {
    let alts = values.iter().map(|v| wrap(v)).collect::<Vec<_>>();
    format!("({})", alts.join(" | "))
}

/// One parameter of a list: its complete Lark item and whether it must be present.
struct Param {
    expr: String,
    required: bool,
}

fn skippable(required: bool, take: String, skip: &Option<String>) -> String {
    match (required, skip) {
        (true, _) => take,
        (false, Some(skip)) => format!("{take} | {skip}"),
        (false, None) => format!("({take})?"),
    }
}

fn then(head: String, tail: &Option<String>) -> String {
    match tail {
        Some(tail) => format!("{head} {tail}"),
        None => head,
    }
}

/// Emit rules for `params` in order, `sep` between present ones, required
/// ones mandatory and optional ones skippable. With `extra`, any number of
/// further members of that shape may follow. Returns the Lark expression for
/// the whole list, or `None` when no parameter can appear.
///
/// `_sN` means "nothing written yet" and `_rN` "something written, so a
/// separator comes first"; both start at parameter N.
fn param_list(
    out: &mut String,
    prefix: &str,
    params: &[Param],
    sep: &str,
    extra: Option<&str>,
) -> Option<String> {
    let with_sep = |expr: &str| {
        if sep.is_empty() {
            expr.to_string()
        } else {
            format!("{sep} {expr}")
        }
    };
    let (mut rest, mut seq) = (None, None);
    if let Some(extra) = extra {
        let _ = writeln!(out, "{prefix}_more: ({})*", with_sep(extra));
        let _ = writeln!(out, "{prefix}_first: {extra} {prefix}_more");
        rest = Some(format!("{prefix}_more"));
        seq = Some(format!("{prefix}_first?"));
    }
    for (k, param) in params.iter().enumerate().rev() {
        let take = then(with_sep(&param.expr), &rest);
        let _ = writeln!(
            out,
            "{prefix}_r{k}: {}",
            skippable(param.required, take, &rest)
        );
        let take = then(param.expr.clone(), &rest);
        let _ = writeln!(
            out,
            "{prefix}_s{k}: {}",
            skippable(param.required, take, &seq)
        );
        rest = Some(format!("{prefix}_r{k}"));
        seq = Some(format!("{prefix}_s{k}"));
    }
    seq
}

/// Terminal for the name of an undeclared parameter of tool `i`: any
/// identifier except the declared ones, which must follow their own rule.
fn extra_name(out: &mut String, i: usize, schema: &Value) -> String {
    let names = declared(schema)
        .iter()
        .map(|d| lark_regex_escape(d.name))
        .collect::<Vec<_>>();
    let rule = format!("XN{i}");
    let ident = r"/[A-Za-z_][A-Za-z0-9_\-]*/";
    if names.is_empty() {
        let _ = writeln!(out, "{rule}: {ident}");
    } else {
        let _ = writeln!(out, "{rule}: {ident} & ~/({})/", names.join("|"));
    }
    rule
}

fn slot(list: Option<String>) -> String {
    list.map(|list| format!("{list} ")).unwrap_or_default()
}

/// Qwen XML-function format, after `<tool_call>`.
/// Mirrors `qwen_function_tool_grammar`:
///
/// ```text
/// toolcall ::= "<tool_call>\n" tool-alt "</tool_call>"   (one or more)
/// call     ::= "<function=NAME>\n" param* "</function>\n"
/// param    ::= "<parameter=K>\n" value "\n</parameter>\n"
/// ```
///
/// String values end at the first `\n</parameter>\n` and may not contain
/// `</parameter>` at all, exactly like the GBNF `value-text` exclusion.
pub(crate) fn qwen_function_lark(tools: &[Tool], tag: Tag<'_>, tier: Tier) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "start: \"\\n\" alt close_t (open_t \"\\n\" alt close_t)*"
    );
    let _ = writeln!(out, "open_t: {}", tag("<tool_call>").expr);
    let _ = writeln!(out, "close_t: {}", tag("</tool_call>").expr);
    let _ = writeln!(out, "alt: {}", alternatives(tools.len()).replace('c', "f"));
    let close = lark_string("\n</parameter>\n");
    text_rule(
        &mut out,
        "text_value",
        "</parameter>",
        &Delim::literal("\n</parameter>\n"),
        "\n</parameter>\n",
    );
    push_terminals(&mut out, tier, true);

    for (i, tool) in tools.iter().enumerate() {
        let schema = &tool.function.parameters;
        let params: Vec<Param> = declared(schema)
            .into_iter()
            .map(|d| {
                let open = lark_string(&format!("<parameter={}>\n", d.name));
                let value = match classify(d.schema) {
                    Kind::Text | Kind::Any => None,
                    Kind::Enum(values) => Some(enum_expr(&values, lark_string)),
                    Kind::Number => Some("NUMBER".to_string()),
                    Kind::Bool => Some("(\"true\" | \"false\")".to_string()),
                    Kind::Null => Some("\"null\"".to_string()),
                    Kind::Array => Some(match tier {
                        Tier::Schema => json_schema_expr(d.schema),
                        Tier::Generic => "jarray".to_string(),
                    }),
                    Kind::Object => Some(match tier {
                        Tier::Schema => json_schema_expr(d.schema),
                        Tier::Generic => "jobject".to_string(),
                    }),
                };
                Param {
                    expr: match value {
                        None => format!("{open} text_value"),
                        Some(value) => format!("{open} {value} {close}"),
                    },
                    required: d.required,
                }
            })
            .collect();
        let extra = (tier == Tier::Schema && extras_allowed(schema)).then(|| {
            let name = extra_name(&mut out, i, schema);
            format!("\"<parameter=\" {name} \">\\n\" text_value")
        });
        let list = param_list(&mut out, &format!("f{i}"), &params, "", extra.as_deref());
        let _ = writeln!(
            out,
            "f{i}: {} {}{}",
            lark_string(&format!("<function={}>\n", tool.function.name)),
            slot(list),
            lark_string("</function>\n"),
        );
    }
    Some(out)
}

/// Ministral format, after `[TOOL_CALLS]`: `tool-name "[ARGS]" object ws`,
/// one or more calls. The schema tier constrains each tool's arguments to
/// its own parameter schema.
pub(crate) fn ministral_lark(tools: &[Tool], tag: Tag<'_>, tier: Tier) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "start: call (open_t call)*");
    let _ = writeln!(out, "open_t: {}", tag("[TOOL_CALLS]").expr);
    let _ = writeln!(out, "args_t: {}", tag("[ARGS]").expr);
    push_terminals(&mut out, tier, true);
    match tier {
        Tier::Generic => {
            let _ = writeln!(out, "call: name args_t jobject WS?");
            let _ = writeln!(out, "name: {}", name_alternatives(tools));
        }
        Tier::Schema => {
            let _ = writeln!(out, "call: {}", alternatives(tools.len()));
            for (i, tool) in tools.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "c{i}: {} args_t {} WS?",
                    lark_string(&tool.function.name),
                    json_schema_expr(&object_schema(tool))
                );
            }
        }
    }
    Some(out)
}

/// Qwen3 JSON format, after `<tool_call>`:
/// `{"name": NAME, "arguments": ARGS}` then `</tool_call>`, repeatable.
pub(crate) fn qwen3_lark(tools: &[Tool], tag: Tag<'_>, tier: Tier) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "start: call (open_t call)*");
    let _ = writeln!(out, "open_t: {}", tag("<tool_call>").expr);
    let _ = writeln!(out, "close_t: {}", tag("</tool_call>").expr);
    push_terminals(&mut out, tier, true);
    let name_key = lark_string("\"name\"");
    let args_key = lark_string("\"arguments\"");
    let _ = writeln!(out, "head: WS? \"{{\" WS? {name_key} WS? \":\" WS?");
    let _ = writeln!(out, "mid: WS? \",\" WS? {args_key} WS? \":\" WS?");
    let _ = writeln!(out, "tail: WS? \"}}\" WS? close_t WS?");
    let quoted = |name: &str| lark_string(&serde_json::to_string(name).expect("name serializes"));
    match tier {
        Tier::Generic => {
            let names = tools
                .iter()
                .map(|tool| quoted(&tool.function.name))
                .collect::<Vec<_>>()
                .join(" | ");
            let _ = writeln!(out, "call: head name mid jobject tail");
            let _ = writeln!(out, "name: {names}");
        }
        Tier::Schema => {
            let _ = writeln!(out, "call: {}", alternatives(tools.len()));
            for (i, tool) in tools.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "c{i}: head {} mid {} tail",
                    quoted(&tool.function.name),
                    json_schema_expr(&object_schema(tool))
                );
            }
        }
    }
    Some(out)
}

/// LFM2 format, after `<|tool_call_start|>`:
/// `[name(k=v, ...)]<|tool_call_end|>`. Values are JSON.
pub(crate) fn lfm2_lark(tools: &[Tool], tag: Tag<'_>, tier: Tier) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut out = String::new();
    let _ = writeln!(out, "start: \"[\" call (\",\" WS? call)* \"]\" end_t WS?");
    let _ = writeln!(out, "end_t: {}", tag("<|tool_call_end|>").expr);
    push_terminals(&mut out, tier, true);
    match tier {
        Tier::Generic => {
            let _ = writeln!(out, "call: name \"(\" WS? kwargs? WS? \")\"");
            let _ = writeln!(out, "kwargs: kwarg (\",\" WS? kwarg)*");
            let _ = writeln!(out, "kwarg: IDENT WS? \"=\" WS? jvalue");
            let _ = writeln!(out, "name: {}", name_alternatives(tools));
        }
        Tier::Schema => {
            let _ = writeln!(out, "call: {}", alternatives(tools.len()));
            for (i, tool) in tools.iter().enumerate() {
                let schema = &tool.function.parameters;
                let params: Vec<Param> = declared(schema)
                    .into_iter()
                    .map(|d| Param {
                        expr: format!(
                            "{} WS? \"=\" WS? {}",
                            lark_string(d.name),
                            json_value_expr(d.schema, tier)
                        ),
                        required: d.required,
                    })
                    .collect();
                let extra = extras_allowed(schema).then(|| {
                    let name = extra_name(&mut out, i, schema);
                    format!("{name} WS? \"=\" WS? %json {{}}")
                });
                let list = param_list(
                    &mut out,
                    &format!("c{i}"),
                    &params,
                    "\",\" WS?",
                    extra.as_deref(),
                );
                let _ = writeln!(
                    out,
                    "c{i}: {} \"(\" WS? {}WS? \")\"",
                    lark_string(&tool.function.name),
                    slot(list)
                );
            }
        }
    }
    Some(out)
}

/// FunctionGemma format, after `<start_function_call>`:
/// `call:NAME{k:<escape>TEXT<escape>,...}<end_function_call>`.
pub(crate) fn function_gemma_lark(tools: &[Tool], tag: Tag<'_>, tier: Tier) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let escape = tag("<escape>");
    let mut out = String::new();
    let _ = writeln!(out, "start: call (open_t call)*");
    let _ = writeln!(out, "open_t: {}", tag("<start_function_call>").expr);
    let _ = writeln!(out, "end_t: {}", tag("<end_function_call>").expr);
    let _ = writeln!(out, "esc_t: {}", escape.expr);
    text_rule(&mut out, "esc_text", "<escape>", &escape, "<escape>");
    push_terminals(&mut out, tier, false);
    let text_member = "WS? \":\" WS? esc_t esc_text";
    match tier {
        Tier::Generic => {
            let _ = writeln!(
                out,
                "call: \"call:\" name \"{{\" WS? members? WS? \"}}\" end_t WS?"
            );
            let _ = writeln!(out, "members: member (\",\" WS? member)*");
            let _ = writeln!(out, "member: IDENT {text_member}");
            let _ = writeln!(out, "name: {}", name_alternatives(tools));
        }
        Tier::Schema => {
            let _ = writeln!(out, "call: {}", alternatives(tools.len()));
            for (i, tool) in tools.iter().enumerate() {
                let schema = &tool.function.parameters;
                let params: Vec<Param> = declared(schema)
                    .into_iter()
                    .map(|d| {
                        let value = match classify(d.schema) {
                            Kind::Text | Kind::Any => "esc_t esc_text".to_string(),
                            Kind::Enum(values) => {
                                format!("esc_t {} esc_t", enum_expr(&values, lark_string))
                            }
                            Kind::Number => "esc_t NUMBER esc_t".to_string(),
                            Kind::Bool => "esc_t (\"true\" | \"false\") esc_t".to_string(),
                            Kind::Null => "esc_t \"null\" esc_t".to_string(),
                            Kind::Array | Kind::Object => {
                                format!("esc_t {} esc_t", json_value_expr(d.schema, tier))
                            }
                        };
                        Param {
                            expr: format!("{} WS? \":\" WS? {value}", lark_string(d.name)),
                            required: d.required,
                        }
                    })
                    .collect();
                let extra = extras_allowed(schema).then(|| {
                    let name = extra_name(&mut out, i, schema);
                    format!("{name} {text_member}")
                });
                let list = param_list(
                    &mut out,
                    &format!("c{i}"),
                    &params,
                    "\",\" WS?",
                    extra.as_deref(),
                );
                let _ = writeln!(
                    out,
                    "c{i}: \"call:\" {} \"{{\" WS? {}WS? \"}}\" end_t WS?",
                    lark_string(&tool.function.name),
                    slot(list)
                );
            }
        }
    }
    Some(out)
}

/// Gemma 4 format, after `<|tool_call>`:
/// `call:NAME{k:V,...}<tool_call|>` with `<|"|>` delimited strings and
/// recursive objects and arrays. The schema tier types the top-level
/// parameters; nested values stay structural (proposal, non-goals).
pub(crate) fn gemma4_lark(tools: &[Tool], tag: Tag<'_>, tier: Tier) -> Option<String> {
    const QUOTE: &str = "<|\"|>";
    if tools.is_empty() {
        return None;
    }
    let quote = tag(QUOTE);
    let mut out = String::new();
    let _ = writeln!(out, "start: one (open_t one)*");
    let _ = writeln!(out, "one: \"call:\" call end_t WS?");
    let _ = writeln!(out, "open_t: {}", tag("<|tool_call>").expr);
    let _ = writeln!(out, "end_t: {}", tag("<tool_call|>").expr);
    let _ = writeln!(out, "q_t: {}", quote.expr);
    text_rule(&mut out, "gtext", QUOTE, &quote, QUOTE);
    push_terminals(&mut out, tier, false);
    out.push_str(
        "gvalue: gobject | garray | gstring | NUMBER | \"true\" | \"false\" | \"null\"\n\
         gstring: q_t gtext\n\
         gobject: \"{\" WS? (gmember (\",\" WS? gmember)*)? WS? \"}\"\n\
         gmember: IDENT WS? \":\" WS? gvalue\n\
         garray: \"[\" WS? (gvalue (\",\" WS? gvalue)*)? WS? \"]\"\n",
    );
    match tier {
        Tier::Generic => {
            let _ = writeln!(out, "call: name gobject");
            let _ = writeln!(out, "name: {}", name_alternatives(tools));
        }
        Tier::Schema => {
            let _ = writeln!(out, "call: {}", alternatives(tools.len()));
            for (i, tool) in tools.iter().enumerate() {
                let schema = &tool.function.parameters;
                let params: Vec<Param> = declared(schema)
                    .into_iter()
                    .map(|d| {
                        let value = match classify(d.schema) {
                            Kind::Text => "gstring".to_string(),
                            Kind::Any => "gvalue".to_string(),
                            Kind::Enum(values) => {
                                enum_expr(&values, |v| format!("q_t {} q_t", lark_string(v)))
                            }
                            Kind::Number => "NUMBER".to_string(),
                            Kind::Bool => "(\"true\" | \"false\")".to_string(),
                            Kind::Null => "\"null\"".to_string(),
                            Kind::Array => "garray".to_string(),
                            Kind::Object => "gobject".to_string(),
                        };
                        Param {
                            expr: format!("{} WS? \":\" WS? {value}", lark_string(d.name)),
                            required: d.required,
                        }
                    })
                    .collect();
                let extra = extras_allowed(schema).then(|| {
                    let name = extra_name(&mut out, i, schema);
                    format!("{name} WS? \":\" WS? gvalue")
                });
                let list = param_list(
                    &mut out,
                    &format!("c{i}"),
                    &params,
                    "\",\" WS?",
                    extra.as_deref(),
                );
                let _ = writeln!(
                    out,
                    "c{i}: {} \"{{\" WS? {}WS? \"}}\"",
                    lark_string(&tool.function.name),
                    slot(list)
                );
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_format::{ToolFormat, parse_assistant_format_with_tools};
    use crate::common_chat::ReasoningFormat;
    use llguidance::api::TopLevelGrammar;
    use llguidance::{Matcher, ParserFactory};
    use querymt::chat::FunctionTool;
    use serde_json::json;
    use toktrie::{ApproximateTokEnv, TokEnv, TokRxInfo, TokTrie};

    fn tool(name: &str, parameters: Value) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionTool {
                name: name.to_string(),
                description: String::new(),
                parameters,
                strict: None,
            },
        }
    }

    fn tools() -> Vec<Tool> {
        vec![
            tool(
                "write_file",
                json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                    "required": ["path", "content"]
                }),
            ),
            tool(
                "glob",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string"},
                        "limit": {"type": "integer"},
                        "hidden": {"type": "boolean"},
                        "mode": {"type": "string", "enum": ["fast", "full"]}
                    },
                    "required": ["pattern"]
                }),
            ),
        ]
    }

    /// `glob`-like tool that forbids undeclared parameters.
    fn closed_tools() -> Vec<Tool> {
        vec![tool(
            "glob",
            json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )]
    }

    /// Byte-level vocabulary: one token per byte, plus the given special
    /// tokens (`0xFF`-prefixed in the trie, like a real vocabulary).
    struct Vocab {
        env: TokEnv,
        specials: &'static [&'static str],
    }

    impl Vocab {
        fn new(specials: &'static [&'static str]) -> Self {
            let mut words: Vec<Vec<u8>> = (0..=255u8).map(|b| vec![b]).collect();
            words.extend(
                specials
                    .iter()
                    .map(|s| [&[0xFF][..], s.as_bytes()].concat()),
            );
            words.push(b"\xFF<|eos|>".to_vec());
            let info = TokRxInfo::new(words.len() as u32, words.len() as u32 - 1);
            let env: TokEnv =
                std::sync::Arc::new(ApproximateTokEnv::new(TokTrie::from(&info, &words)));
            Self { env, specials }
        }

        fn tag(&self) -> impl Fn(&str) -> Delim + '_ {
            move |text| match self.specials.iter().position(|s| *s == text) {
                Some(i) => Delim::special(256 + i as u32, text),
                None => Delim::literal(text),
            }
        }

        /// Special tokens are recognised by their spelling.
        fn tokens(&self, text: &str) -> Vec<u32> {
            let bytes = text.as_bytes();
            let mut out = Vec::new();
            let mut i = 0;
            'next: while i < bytes.len() {
                for (k, special) in self.specials.iter().enumerate() {
                    if bytes[i..].starts_with(special.as_bytes()) {
                        out.push(256 + k as u32);
                        i += special.len();
                        continue 'next;
                    }
                }
                out.push(u32::from(bytes[i]));
                i += 1;
            }
            out
        }

        /// True when the whole text is accepted and the grammar can end there.
        fn accepts(&self, lark: &str, text: &str) -> bool {
            let mut factory = ParserFactory::new_simple(&self.env).unwrap();
            factory.quiet();
            let mut m =
                Matcher::new(factory.create_parser(TopLevelGrammar::from_lark(lark.to_string())));
            assert!(m.get_error().is_none(), "{:?}\n{lark}", m.get_error());
            let tokens = self.tokens(text);
            let n = m.try_consume_tokens(&tokens).unwrap_or(0);
            n == tokens.len() && m.is_accepting().unwrap_or(false)
        }
    }

    /// One tool-call format with how its calls are written.
    struct Case {
        format: ToolFormat,
        trigger: &'static str,
        specials: &'static [&'static str],
    }

    const CASES: [Case; 6] = [
        Case {
            format: ToolFormat::Qwen3,
            trigger: "<tool_call>",
            specials: &[],
        },
        Case {
            format: ToolFormat::QwenFunction,
            trigger: "<tool_call>",
            specials: &[],
        },
        Case {
            format: ToolFormat::Ministral3,
            trigger: "[TOOL_CALLS]",
            specials: &[],
        },
        Case {
            format: ToolFormat::Lfm2,
            trigger: "<|tool_call_start|>",
            specials: &["<|tool_call_end|>"],
        },
        Case {
            format: ToolFormat::FunctionGemma,
            trigger: "<start_function_call>",
            specials: &["<end_function_call>", "<escape>", "<start_function_call>"],
        },
        Case {
            format: ToolFormat::Gemma4,
            trigger: "<|tool_call>",
            specials: &["<tool_call|>", "<|\"|>", "<|tool_call>"],
        },
    ];

    fn raw(value: &Value) -> String {
        match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }

    impl Case {
        fn lark(&self, vocab: &Vocab, tools: &[Tool], tier: Tier) -> String {
            let tag = vocab.tag();
            let lark = match self.format {
                ToolFormat::Qwen3 => qwen3_lark(tools, &tag, tier),
                ToolFormat::QwenFunction => qwen_function_lark(tools, &tag, tier),
                ToolFormat::Ministral3 => ministral_lark(tools, &tag, tier),
                ToolFormat::Lfm2 => lfm2_lark(tools, &tag, tier),
                ToolFormat::FunctionGemma => function_gemma_lark(tools, &tag, tier),
                ToolFormat::Gemma4 => gemma4_lark(tools, &tag, tier),
            };
            lark.expect("grammar")
        }

        /// The text after the trigger for one call.
        fn call(&self, name: &str, args: &[(&str, Value)]) -> String {
            let object = |args: &[(&str, Value)]| {
                Value::Object(
                    args.iter()
                        .map(|(k, v)| (k.to_string(), v.clone()))
                        .collect(),
                )
            };
            let join = |f: &dyn Fn(&str, &Value) -> String, sep: &str| {
                args.iter()
                    .map(|(k, v)| f(k, v))
                    .collect::<Vec<_>>()
                    .join(sep)
            };
            match self.format {
                ToolFormat::Qwen3 => format!(
                    "\n{{\"name\": \"{name}\", \"arguments\": {}}}\n</tool_call>",
                    object(args)
                ),
                ToolFormat::QwenFunction => format!(
                    "\n<function={name}>\n{}</function>\n</tool_call>",
                    join(
                        &|k, v| format!("<parameter={k}>\n{}\n</parameter>\n", raw(v)),
                        ""
                    )
                ),
                ToolFormat::Ministral3 => format!("{name}[ARGS]{}", object(args)),
                ToolFormat::Lfm2 => format!(
                    "[{name}({})]<|tool_call_end|>",
                    join(&|k, v| format!("{k}={v}"), ", ")
                ),
                ToolFormat::FunctionGemma => format!(
                    "call:{name}{{{}}}<end_function_call>",
                    join(&|k, v| format!("{k}:<escape>{}<escape>", raw(v)), ",")
                ),
                ToolFormat::Gemma4 => format!(
                    "call:{name}{{{}}}<tool_call|>",
                    join(
                        &|k, v| match v {
                            Value::String(s) => format!("{k}:<|\"|>{s}<|\"|>"),
                            other => format!("{k}:{other}"),
                        },
                        ","
                    )
                ),
            }
        }

        /// Arguments the provider parses out of `trigger + body`.
        fn parsed(&self, tools: &[Tool], body: &str) -> Option<Value> {
            let text = format!("{}{body}", self.trigger);
            let parsed =
                parse_assistant_format_with_tools(&text, ReasoningFormat::ThinkTags, false, tools);
            let calls = parsed.tool_calls?;
            serde_json::from_str(&calls.first()?.function.arguments).ok()
        }
    }

    fn each_case(mut check: impl FnMut(&Case, &Vocab)) {
        for case in &CASES {
            check(case, &Vocab::new(case.specials));
        }
    }

    fn glob_args() -> Vec<(&'static str, Value)> {
        vec![
            ("pattern", json!("*.rs")),
            ("limit", json!(10)),
            ("hidden", json!(true)),
            ("mode", json!("fast")),
        ]
    }

    #[test]
    fn every_format_accepts_a_valid_call_in_both_tiers() {
        let tools = tools();
        each_case(|case, vocab| {
            for tier in [Tier::Schema, Tier::Generic] {
                let lark = case.lark(vocab, &tools, tier);
                for (name, args) in [
                    ("glob", glob_args()),
                    ("glob", vec![("pattern", json!("*"))]),
                    (
                        "write_file",
                        vec![("path", json!("a")), ("content", json!("b"))],
                    ),
                ] {
                    let body = case.call(name, &args);
                    assert!(
                        vocab.accepts(&lark, &body),
                        "{:?} {tier:?} rejects {body:?}\n{lark}",
                        case.format
                    );
                    let expected = Value::Object(
                        args.iter()
                            .map(|(k, v)| (k.to_string(), v.clone()))
                            .collect(),
                    );
                    assert_eq!(
                        case.parsed(&tools, &body),
                        Some(expected),
                        "{:?}: {body:?}",
                        case.format
                    );
                }
            }
        });
    }

    #[test]
    fn every_format_accepts_several_calls() {
        let tools = tools();
        each_case(|case, vocab| {
            let a = case.call("glob", &[("pattern", json!("*"))]);
            let b = case.call(
                "write_file",
                &[("path", json!("a")), ("content", json!("b"))],
            );
            // Between calls the trigger is written again.
            let body = format!("{a}{}{b}", case.trigger);
            for tier in [Tier::Schema, Tier::Generic] {
                let lark = case.lark(vocab, &tools, tier);
                let multi = !matches!(case.format, ToolFormat::Lfm2);
                assert_eq!(
                    vocab.accepts(&lark, &body),
                    multi,
                    "{:?} {tier:?}",
                    case.format
                );
            }
        });
    }

    #[test]
    fn schema_rejects_wrong_value_type() {
        let tools = tools();
        each_case(|case, vocab| {
            let mut bad = glob_args();
            bad[1] = ("limit", json!("ten"));
            let (ok, bad) = (case.call("glob", &glob_args()), case.call("glob", &bad));
            let schema = case.lark(vocab, &tools, Tier::Schema);
            assert!(vocab.accepts(&schema, &ok), "{:?}", case.format);
            assert!(!vocab.accepts(&schema, &bad), "{:?}: {bad:?}", case.format);
            // The generic tier does not know types (Qwen XML-function's
            // GBNF already does), so this test fails without the schema tier.
            if case.format != ToolFormat::QwenFunction {
                let generic = case.lark(vocab, &tools, Tier::Generic);
                assert!(vocab.accepts(&generic, &bad), "{:?}", case.format);
            }
        });
    }

    #[test]
    fn schema_rejects_value_outside_enum() {
        let tools = tools();
        each_case(|case, vocab| {
            let with =
                |mode: &str| case.call("glob", &[("pattern", json!("*")), ("mode", json!(mode))]);
            let schema = case.lark(vocab, &tools, Tier::Schema);
            for ok in ["fast", "full"] {
                assert!(vocab.accepts(&schema, &with(ok)), "{:?} {ok}", case.format);
            }
            for bad in ["slow", "fas", "fastfull", ""] {
                assert!(
                    !vocab.accepts(&schema, &with(bad)),
                    "{:?} {bad:?}",
                    case.format
                );
            }
        });
    }

    #[test]
    fn schema_requires_required_parameters() {
        let tools = tools();
        each_case(|case, vocab| {
            let schema = case.lark(vocab, &tools, Tier::Schema);
            for missing in [
                vec![],
                vec![("limit", json!(1))],
                vec![("mode", json!("fast"))],
            ] {
                let body = case.call("glob", &missing);
                assert!(
                    !vocab.accepts(&schema, &body),
                    "{:?}: {body:?}",
                    case.format
                );
            }
            // Optional parameters may be skipped.
            for present in [
                vec![("pattern", json!("*"))],
                vec![("pattern", json!("*")), ("mode", json!("full"))],
                vec![("pattern", json!("*")), ("hidden", json!(false))],
                vec![
                    ("pattern", json!("*")),
                    ("limit", json!(3)),
                    ("mode", json!("fast")),
                ],
            ] {
                let body = case.call("glob", &present);
                assert!(vocab.accepts(&schema, &body), "{:?}: {body:?}", case.format);
            }
        });
    }

    #[test]
    fn schema_rejects_undeclared_parameter_when_additional_properties_are_forbidden() {
        let extra = [("pattern", json!("*")), ("zzz", json!("1"))];
        each_case(|case, vocab| {
            let body = case.call("glob", &extra);
            let closed = case.lark(vocab, &closed_tools(), Tier::Schema);
            assert!(
                !vocab.accepts(&closed, &body),
                "{:?}: {body:?}",
                case.format
            );
            // Control: without the restriction undeclared names are allowed.
            let open = case.lark(vocab, &tools(), Tier::Schema);
            assert!(vocab.accepts(&open, &body), "{:?}: {body:?}", case.format);
        });
    }

    #[test]
    fn schema_rejects_unknown_tool_name() {
        let tools = tools();
        each_case(|case, vocab| {
            let body = case.call("nope", &[("pattern", json!("*"))]);
            for tier in [Tier::Schema, Tier::Generic] {
                let lark = case.lark(vocab, &tools, tier);
                assert!(!vocab.accepts(&lark, &body), "{:?} {tier:?}", case.format);
            }
        });
    }

    #[test]
    fn value_accepts_less_than_and_markup() {
        let tools = tools();
        each_case(|case, vocab| {
            // Formats whose strings carry raw text.
            if !matches!(
                case.format,
                ToolFormat::QwenFunction | ToolFormat::FunctionGemma | ToolFormat::Gemma4
            ) {
                return;
            }
            let lark = case.lark(vocab, &tools, Tier::Schema);
            for content in [
                "if row[j] < best_d:\n    pass",
                "a <= b and c<d",
                "<div></div>",
                "</param",
                "x\n</parameter",
                "<escape",
                "<|\"|",
                "",
            ] {
                let body = case.call(
                    "write_file",
                    &[("path", json!("a.py")), ("content", json!(content))],
                );
                assert!(
                    vocab.accepts(&lark, &body),
                    "{:?}: {content:?}\n{lark}",
                    case.format
                );
            }
        });
    }

    #[test]
    fn value_cannot_contain_its_terminator() {
        let tools = tools();
        each_case(|case, vocab| {
            let terminator = match case.format {
                ToolFormat::QwenFunction => "</parameter>",
                ToolFormat::FunctionGemma => "<escape>",
                ToolFormat::Gemma4 => "<|\"|>",
                _ => return,
            };
            let lark = case.lark(vocab, &tools, Tier::Schema);
            let body = case.call(
                "write_file",
                &[
                    ("path", json!("a")),
                    ("content", json!(format!("x{terminator}y"))),
                ],
            );
            assert!(!vocab.accepts(&lark, &body), "{:?}", case.format);
        });
    }

    #[test]
    fn qwen_function_enforces_schema_and_required_params() {
        let vocab = Vocab::new(&[]);
        let lark = CASES[1].lark(&vocab, &tools(), Tier::Schema);
        let ok = "\n<function=glob>\n<parameter=pattern>\n*.rs\n</parameter>\n<parameter=limit>\n10\n</parameter>\n<parameter=mode>\nfast\n</parameter>\n</function>\n</tool_call>";
        assert!(vocab.accepts(&lark, ok));
        for bad in [
            ok.replace("\n10\n", "\nten\n"),
            ok.replace("\nfast\n", "\nslow\n"),
            "\n<function=glob>\n</function>\n</tool_call>".to_string(),
            "\n<function=nope>\n</function>\n</tool_call>".to_string(),
        ] {
            assert!(!vocab.accepts(&lark, &bad), "{bad:?}");
        }
        assert!(vocab.accepts(&lark, &format!("{ok}<tool_call>{ok}")));
    }

    #[test]
    fn ministral_grammar_compiles_and_matches() {
        let vocab = Vocab::new(&[]);
        let lark = CASES[2].lark(&vocab, &tools(), Tier::Schema);
        assert!(vocab.accepts(&lark, "glob[ARGS]{\"pattern\": \"*.rs\"}"));
        assert!(vocab.accepts(
            &lark,
            "glob[ARGS]{\"pattern\": \"*\"}[TOOL_CALLS]write_file[ARGS]{\"path\": \"a\", \"content\": \"b\"}"
        ));
        assert!(!vocab.accepts(&lark, "nope[ARGS]{}"));
        assert!(!vocab.accepts(&lark, "write_file[ARGS]{}"));
        assert!(!vocab.accepts(&lark, "glob[ARGS]{\"pattern\": \"*\", \"limit\": \"ten\"}"));
        assert!(!vocab.accepts(&lark, "glob[ARGS]{\"pattern\": \"*\", \"mode\": \"slow\"}"));
        let generic = CASES[2].lark(&vocab, &tools(), Tier::Generic);
        assert!(vocab.accepts(&generic, "write_file[ARGS]{}"));
    }

    #[test]
    fn regex_escape_covers_terminators() {
        let vocab = Vocab::new(&[]);
        for terminator in ["</parameter>", "<escape>", "<|\"|>", "a.b*c"] {
            let lark = format!("start: X\n{}", exclusion_terminal("X", terminator));
            assert!(vocab.accepts(&lark, "plain text"), "{lark}");
            assert!(!vocab.accepts(&lark, &format!("a{terminator}b")), "{lark}");
        }
    }

    #[test]
    fn gemma4_nested_values_stay_structural() {
        let vocab = Vocab::new(&["<tool_call|>", "<|\"|>"]);
        let tools = vec![tool(
            "run",
            json!({
                "type": "object",
                "properties": {"opts": {"type": "object"}, "xs": {"type": "array"}},
                "required": ["opts"]
            }),
        )];
        let lark = CASES[5].lark(&vocab, &tools, Tier::Schema);
        for ok in [
            "call:run{opts:{a:<|\"|>x<|\"|>,b:[1,2,{c:true}]}}<tool_call|>",
            "call:run{opts:{},xs:[<|\"|>a<|\"|>]}<tool_call|>",
        ] {
            assert!(vocab.accepts(&lark, ok), "{ok}");
        }
        for bad in [
            "call:run{opts:<|\"|>s<|\"|>}<tool_call|>",
            "call:run{xs:[]}<tool_call|>",
        ] {
            assert!(!vocab.accepts(&lark, bad), "{bad}");
        }
    }
}
