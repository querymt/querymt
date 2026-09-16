//! Per-file parsers for `.agents` Protocol documents (task 1.5).
//!
//! Each parser turns a single protocol file into a typed model from
//! [`super::manifest`]. Parsers are pure: they take already-read text (plus the
//! [`DotagentsSource`] it came from) and return either a typed value or a
//! source-aware [`DotagentsDiagnostic`]. They never touch the filesystem, start
//! processes, or persist anything, so callers can preview a manifest without
//! side effects.
//!
//! The protocol defines these file kinds:
//!
//! | File | Kind | Model |
//! | --- | --- | --- |
//! | `agents.md` | Markdown singleton | [`DotagentsPrompt`] |
//! | `system-prompt.md` | Markdown singleton | [`DotagentsPrompt`] |
//! | `mcp.json` | JSON (`mcpServers`) | [`DotagentsMcpServer`] |
//! | `models.json` | JSON presets | [`DotagentsModelPreset`] |
//! | `agents/<id>/agent.md` (+ `config.json`) | Markdown + JSON | [`DotagentsAgent`] |
//! | `tasks/<id>/task.md` | Markdown | [`DotagentsTask`] |
//! | `memories/<id>.md` | Markdown | [`DotagentsMemory`] |
//!
//! Unknown YAML/JSON fields are retained in each model's `extensions` map so
//! forward-compatible documents remain usable (see the "Markdown and JSON
//! parsing is protocol compatible" requirement).

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::frontmatter::{
    field_bool, field_list, field_string, field_u64, parse_frontmatter_markdown,
};
use super::layer::DotagentsSource;
use super::manifest::{
    DotagentsAgent, DotagentsAgentConfig, DotagentsAgentConnection, DotagentsAgentConnectionType,
    DotagentsAgentRole, DotagentsMcpServer, DotagentsMcpTransport, DotagentsMemory,
    DotagentsModelPreset, DotagentsPrompt, DotagentsSkill, DotagentsTask, DotagentsTaskKind,
};
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Frontmatter keys recognized by a parser, in canonical (lowercase) form.
///
/// Anything not listed here is an extension field and is retained.
fn is_extension_key(known: &BTreeSet<&str>, key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    !known
        .iter()
        .any(|known_key| known_key.to_ascii_lowercase() == key)
}

/// Split parsed frontmatter into recognized protocol fields and extension
/// fields, keyed case-insensitively for the recognized set.
///
/// The first matching key wins for a recognized field, so a document that
/// spells the same concept twice keeps deterministic behavior.
///
/// Recognized fields are re-keyed to the canonical spelling supplied in
/// `known`, because the [`super::frontmatter`] field accessors look keys up
/// verbatim. Protocol documents use camelCase (`intervalMinutes`,
/// `runOnStartup`), so a case-insensitive match must still be stored under the
/// accessor's expected spelling.
fn partition_fields(
    fields: &BTreeMap<String, Value>,
    known: &[&str],
) -> (BTreeMap<String, Value>, BTreeMap<String, Value>) {
    let mut recognized: BTreeMap<String, Value> = BTreeMap::new();
    let mut extensions: BTreeMap<String, Value> = BTreeMap::new();

    for (key, value) in fields {
        match known
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(key))
        {
            Some(canonical) => {
                recognized
                    .entry((*canonical).to_string())
                    .or_insert_with(|| value.clone());
            }
            None => {
                extensions.insert(key.clone(), value.clone());
            }
        }
    }
    (recognized, extensions)
}

/// Fields recognized by the Markdown singleton parsers.
const PROMPT_KEYS: &[&str] = &[];

/// Parse a Markdown singleton document (`agents.md` or `system-prompt.md`).
///
/// The protocol frontmatter is retained as metadata and never becomes part of
/// the prompt body, so callers can append the body directly.
pub fn parse_prompt_document(
    text: &str,
    source: DotagentsSource,
) -> Result<DotagentsPrompt, Box<DotagentsDiagnostic>> {
    let parsed =
        parse_frontmatter_markdown(text).map_err(|error| error.with_source(source.clone()))?;
    let (_, metadata) = partition_fields(&parsed.fields, PROMPT_KEYS);

    Ok(DotagentsPrompt {
        // Singleton documents have no recognized frontmatter fields, so the
        // full metadata map is retained for inspection.
        metadata: merge_metadata(&parsed.fields, metadata),
        body: parsed.body,
        source,
        fingerprint: fingerprint(text),
    })
}

/// Merge recognized and extension metadata into one inspectable map.
///
/// Singleton prompts retain every frontmatter field; the merge keeps the
/// original spelling and ordering stable.
fn merge_metadata(
    original: &BTreeMap<String, Value>,
    _extensions: BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    original.clone()
}

/// A content fingerprint of the raw source text.
///
/// Fingerprints are stable across runs and platforms so they can be used as
/// reconciliation and approval keys. SHA-256 keeps persisted fingerprints
/// deterministic across processes and platforms.
pub fn fingerprint(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Parse a JSON document into an object, producing a source-aware diagnostic.
fn parse_json_object(
    text: &str,
    source: &DotagentsSource,
) -> Result<serde_json::Map<String, Value>, Box<DotagentsDiagnostic>> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!("invalid JSON: {error}"),
            )
            .with_source(source.clone()),
        )
    })?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!(
                    "expected a JSON object at the top level, found {}",
                    json_type_name(&other)
                ),
            )
            .with_source(source.clone()),
        )),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Read a required non-empty string field.
fn required_string(
    entry: &serde_json::Map<String, Value>,
    key: &str,
    source: &DotagentsSource,
    what: &str,
) -> Result<String, Box<DotagentsDiagnostic>> {
    match entry.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        Some(Value::String(_)) | None => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::MissingField,
                format!("{what} requires a non-empty `{key}`"),
            )
            .with_source(source.clone()),
        )),
        Some(other) => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!(
                    "{what} `{key}` must be a string, found {}",
                    json_type_name(other)
                ),
            )
            .with_source(source.clone()),
        )),
    }
}

/// Read an optional string field, ignoring empty values.
fn optional_string(entry: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match entry.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.trim().to_string()),
        _ => None,
    }
}

/// Read a boolean field, defaulting to `true` when absent.
fn bool_or_default(
    entry: &serde_json::Map<String, Value>,
    key: &str,
    source: &DotagentsSource,
    what: &str,
) -> Result<bool, Box<DotagentsDiagnostic>> {
    match entry.get(key) {
        None => Ok(true),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" => Ok(true),
            "false" | "no" => Ok(false),
            _ => Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::ParseError,
                    format!("{what} `{key}` must be a boolean"),
                )
                .with_source(source.clone()),
            )),
        },
        Some(other) => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!(
                    "{what} `{key}` must be a boolean, found {}",
                    json_type_name(other)
                ),
            )
            .with_source(source.clone()),
        )),
    }
}

/// Read a string array field, accepting a single string as a one-element list.
fn string_list(
    entry: &serde_json::Map<String, Value>,
    key: &str,
    source: &DotagentsSource,
    what: &str,
) -> Result<Vec<String>, Box<DotagentsDiagnostic>> {
    match entry.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(value) => out.push(value.clone()),
                    Value::Number(number) => out.push(number.to_string()),
                    other => {
                        return Err(Box::new(
                            DotagentsDiagnostic::error(
                                DotagentsDiagnosticCode::ParseError,
                                format!(
                                    "{what} `{key}` must contain only strings, found {}",
                                    json_type_name(other)
                                ),
                            )
                            .with_source(source.clone()),
                        ));
                    }
                }
            }
            Ok(out)
        }
        Some(Value::String(value)) => {
            if value.trim().is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![value.clone()])
            }
        }
        Some(other) => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!(
                    "{what} `{key}` must be an array of strings, found {}",
                    json_type_name(other)
                ),
            )
            .with_source(source.clone()),
        )),
    }
}

/// Read a string map field (environment variables, headers).
fn string_map(
    entry: &serde_json::Map<String, Value>,
    key: &str,
    source: &DotagentsSource,
    what: &str,
) -> Result<BTreeMap<String, String>, Box<DotagentsDiagnostic>> {
    match entry.get(key) {
        None => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => {
            let mut out = BTreeMap::new();
            for (name, value) in map {
                match value {
                    Value::String(text) => {
                        out.insert(name.clone(), text.clone());
                    }
                    Value::Number(number) => {
                        out.insert(name.clone(), number.to_string());
                    }
                    Value::Bool(flag) => {
                        out.insert(name.clone(), flag.to_string());
                    }
                    other => {
                        return Err(Box::new(
                            DotagentsDiagnostic::error(
                                DotagentsDiagnosticCode::ParseError,
                                format!(
                                    "{what} `{key}` entry `{name}` must be a string, found {}",
                                    json_type_name(other)
                                ),
                            )
                            .with_source(source.clone()),
                        ));
                    }
                }
            }
            Ok(out)
        }
        Some(other) => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!(
                    "{what} `{key}` must be an object, found {}",
                    json_type_name(other)
                ),
            )
            .with_source(source.clone()),
        )),
    }
}

/// Collect unknown keys of a JSON object into an extension map.
fn json_extensions(
    entry: &serde_json::Map<String, Value>,
    known: &[&str],
) -> BTreeMap<String, Value> {
    let known: BTreeSet<&str> = known.iter().copied().collect();
    entry
        .iter()
        .filter(|(key, _)| is_extension_key(&known, key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// `mcp.json`
// ---------------------------------------------------------------------------

/// Fields recognized on an `mcpServers` entry.
const MCP_SERVER_KEYS: &[&str] = &[
    "transport",
    "type",
    "command",
    "args",
    "env",
    "url",
    "headers",
    "enabled",
];

/// The outcome of parsing `mcp.json`: usable servers plus isolated diagnostics.
#[derive(Debug, Default)]
pub struct ParsedMcpDocument {
    /// Successfully parsed enabled-or-disabled server entries keyed by name.
    pub servers: BTreeMap<String, DotagentsMcpServer>,
    /// Diagnostics for entries that could not be represented safely.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

/// Parse an `mcp.json` document.
///
/// Every entry under `mcpServers` is parsed independently, so one invalid
/// server does not hide its siblings (see "Invalid entries produce isolated
/// diagnostics"). An entry that cannot be represented safely is omitted from
/// `servers` and reported through `diagnostics`.
///
/// Transport handling follows the protocol support matrix:
///
/// - an explicit `transport`/`type` spelling is honored;
/// - an omitted transport is inferred only when the entry is unambiguous
///   (`command`-only ⇒ `stdio`, `url`-only ⇒ `streamable-http`);
/// - `command` + `url` without a transport is a conflict and is rejected;
/// - WebSocket and unknown transports are rejected with a source-aware
///   diagnostic because the QueryMT MCP runtime cannot represent them.
pub fn parse_mcp_document(
    text: &str,
    file_source: DotagentsSource,
) -> Result<ParsedMcpDocument, Box<DotagentsDiagnostic>> {
    let object = parse_json_object(text, &file_source)?;
    let mut result = ParsedMcpDocument::default();

    let Some(servers) = object.get("mcpServers") else {
        // A syntactically valid document with no `mcpServers` key contributes
        // no servers; this is not an error.
        return Ok(result);
    };

    let Value::Object(servers) = servers else {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!(
                    "`mcpServers` must be an object, found {}",
                    json_type_name(servers)
                ),
            )
            .with_source(file_source),
        ));
    };

    for (name, value) in servers {
        let source = DotagentsSource::entry(
            file_source.layer,
            file_source.lexical_path.clone(),
            name.clone(),
        );
        match parse_mcp_server(name, value, source) {
            Ok(server) => {
                result.servers.insert(name.clone(), server);
            }
            Err(diagnostic) => result.diagnostics.push(*diagnostic),
        }
    }

    Ok(result)
}

/// Parse a single `mcpServers` entry.
fn parse_mcp_server(
    name: &str,
    value: &Value,
    source: DotagentsSource,
) -> Result<DotagentsMcpServer, Box<DotagentsDiagnostic>> {
    let what = format!("MCP server `{name}`");
    let Value::Object(entry) = value else {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!("{what} must be an object, found {}", json_type_name(value)),
            )
            .with_source(source),
        ));
    };

    let declared_transport =
        optional_string(entry, "transport").or_else(|| optional_string(entry, "type"));
    let command = optional_string(entry, "command");
    let url = optional_string(entry, "url");

    let transport = resolve_transport(
        declared_transport.as_deref(),
        command.as_deref(),
        url.as_deref(),
        &what,
        &source,
    )?;

    if !transport.is_supported() {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::UnsupportedTransport,
                format!(
                    "{what} requests `{}` transport, which the current QueryMT MCP runtime cannot represent",
                    transport
                ),
            )
            .with_source(source),
        ));
    }

    // Enforce transport-specific required fields so an unusable server is not
    // silently carried into the manifest.
    match transport {
        DotagentsMcpTransport::Stdio if command.is_none() => {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::MissingField,
                    format!("{what} uses `stdio` transport and requires a `command`"),
                )
                .with_source(source),
            ));
        }
        DotagentsMcpTransport::StreamableHttp if url.is_none() => {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::MissingField,
                    format!("{what} uses `streamable-http` transport and requires a `url`"),
                )
                .with_source(source),
            ));
        }
        _ => {}
    }

    let args = string_list(entry, "args", &source, &what)?;
    let env = string_map(entry, "env", &source, &what)?;
    let headers = string_map(entry, "headers", &source, &what)?;
    let enabled = bool_or_default(entry, "enabled", &source, &what)?;

    Ok(DotagentsMcpServer {
        name: name.to_string(),
        transport,
        declared_transport,
        command,
        args,
        env,
        url,
        headers,
        enabled,
        extensions: json_extensions(entry, MCP_SERVER_KEYS),
        source,
    })
}

/// Resolve or infer the transport for one MCP entry.
fn resolve_transport(
    declared: Option<&str>,
    command: Option<&str>,
    url: Option<&str>,
    what: &str,
    source: &DotagentsSource,
) -> Result<DotagentsMcpTransport, Box<DotagentsDiagnostic>> {
    if let Some(declared) = declared {
        let transport = DotagentsMcpTransport::parse(declared);
        return match transport {
            DotagentsMcpTransport::Unknown => Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::UnsupportedTransport,
                    format!("{what} requests unknown transport `{declared}`"),
                )
                .with_source(source.clone()),
            )),
            _ => Ok(transport),
        };
    }

    match (command.is_some(), url.is_some()) {
        (true, false) => Ok(DotagentsMcpTransport::Stdio),
        (false, true) => Ok(DotagentsMcpTransport::StreamableHttp),
        (true, true) => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ConflictingFields,
                format!(
                    "{what} defines both `command` and `url` without a `transport`; \
                     provide an unambiguous supported transport configuration"
                ),
            )
            .with_source(source.clone()),
        )),
        (false, false) => Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::MissingField,
                format!(
                    "{what} defines neither `command` nor `url`; \
                     provide an unambiguous supported transport configuration"
                ),
            )
            .with_source(source.clone()),
        )),
    }
}

// ---------------------------------------------------------------------------
// `models.json`
// ---------------------------------------------------------------------------

/// Fields recognized on a `models.json` preset entry.
const MODEL_PRESET_KEYS: &[&str] = &[
    "provider",
    "model",
    "credential",
    "credentials",
    "apiKey",
    "parameters",
    "params",
    "enabled",
];

/// The outcome of parsing `models.json`.
#[derive(Debug, Default)]
pub struct ParsedModelsDocument {
    /// Successfully parsed presets keyed by name.
    pub presets: BTreeMap<String, DotagentsModelPreset>,
    /// Diagnostics for entries that could not be represented safely.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

/// Parse a `models.json` document.
///
/// Presets are read from a top-level `models` object when present, and
/// otherwise from the document's top-level keys, so both shapes of the draft
/// protocol parse predictably. Each preset is parsed independently so one
/// invalid entry does not hide valid siblings.
pub fn parse_models_document(
    text: &str,
    file_source: DotagentsSource,
) -> Result<ParsedModelsDocument, Box<DotagentsDiagnostic>> {
    let object = parse_json_object(text, &file_source)?;
    let mut result = ParsedModelsDocument::default();

    let presets: &serde_json::Map<String, Value> = match object.get("models") {
        Some(Value::Object(models)) => models,
        Some(other) => {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::ParseError,
                    format!(
                        "`models` must be an object, found {}",
                        json_type_name(other)
                    ),
                )
                .with_source(file_source),
            ));
        }
        None => &object,
    };

    for (name, value) in presets {
        let source = DotagentsSource::entry(
            file_source.layer,
            file_source.lexical_path.clone(),
            name.clone(),
        );
        match parse_model_preset(name, value, source) {
            Ok(preset) => {
                result.presets.insert(name.clone(), preset);
            }
            Err(diagnostic) => result.diagnostics.push(*diagnostic),
        }
    }

    Ok(result)
}

/// Parse a single named model preset.
fn parse_model_preset(
    name: &str,
    value: &Value,
    source: DotagentsSource,
) -> Result<DotagentsModelPreset, Box<DotagentsDiagnostic>> {
    let what = format!("model preset `{name}`");
    let Value::Object(entry) = value else {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!("{what} must be an object, found {}", json_type_name(value)),
            )
            .with_source(source),
        ));
    };

    let provider = required_string(entry, "provider", &source, &what)?;
    let model = required_string(entry, "model", &source, &what)?;
    let credential = optional_string(entry, "credential")
        .or_else(|| optional_string(entry, "credentials"))
        .or_else(|| optional_string(entry, "apiKey"));

    // Provider parameters are applied as an overlay. Both `parameters` and
    // `params` are accepted spellings from the draft protocol.
    let mut parameters = BTreeMap::new();
    for key in ["parameters", "params"] {
        match entry.get(key) {
            Some(Value::Object(map)) => {
                for (param, param_value) in map {
                    parameters.insert(param.clone(), param_value.clone());
                }
            }
            Some(other) => {
                return Err(Box::new(
                    DotagentsDiagnostic::error(
                        DotagentsDiagnosticCode::ParseError,
                        format!(
                            "{what} `{key}` must be an object, found {}",
                            json_type_name(other)
                        ),
                    )
                    .with_source(source),
                ));
            }
            None => {}
        }
    }

    Ok(DotagentsModelPreset {
        name: name.to_string(),
        provider,
        model,
        credential,
        parameters,
        extensions: json_extensions(entry, MODEL_PRESET_KEYS),
        source,
    })
}

// ---------------------------------------------------------------------------
// `agents/<id>/agent.md` and adjacent `config.json`
// ---------------------------------------------------------------------------

/// Frontmatter keys recognized on an `agent.md` profile.
///
/// Spellings must match the exact keys the frontmatter accessors look up;
/// [`partition_fields`] re-keys case-insensitive matches to these canonical
/// spellings.
const AGENT_KEYS: &[&str] = &[
    "id",
    "name",
    "description",
    "enabled",
    "role",
    "connection",
    "connectionType",
    "capabilities",
];

/// Keys recognized in an agent's adjacent `config.json`.
const AGENT_CONFIG_KEYS: &[&str] = &["model", "modelpreset", "tools", "mcpservers", "mcp"];

/// The outcome of parsing an agent profile and its optional `config.json`.
#[derive(Debug)]
pub struct ParsedAgentDocument {
    /// The parsed profile, when valid.
    pub agent: Option<DotagentsAgent>,
    /// Diagnostics gathered while parsing the profile or its config.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

/// Parse an `agent.md` profile document.
///
/// `entry_id` is the directory name, used as the stable ID unless the
/// frontmatter provides an explicit `id`. The returned profile is `None` when
/// the document cannot be represented; diagnostics describe why. A profile
/// whose body or metadata is invalid does not affect sibling profiles.
pub fn parse_agent_document(
    text: &str,
    entry_id: &str,
    source: DotagentsSource,
) -> Result<ParsedAgentDocument, Box<DotagentsDiagnostic>> {
    let mut diagnostics = Vec::new();
    let parsed =
        parse_frontmatter_markdown(text).map_err(|error| error.with_source(source.clone()))?;
    let (recognized, extensions) = partition_fields(&parsed.fields, AGENT_KEYS);

    let id = field_string(&recognized, "id").unwrap_or_else(|| entry_id.to_string());
    let name = field_string(&recognized, "name").unwrap_or_else(|| id.clone());
    let description = field_string(&recognized, "description").unwrap_or_default();
    let enabled = field_bool(&recognized, "enabled").unwrap_or(true);

    let role = match field_string(&recognized, "role") {
        Some(role) => DotagentsAgentRole::parse(&role),
        None => DotagentsAgentRole::DelegationTarget,
    };

    let connection = parse_agent_connection(&recognized);

    let capabilities = field_list(&recognized, "capabilities").unwrap_or_default();

    if description.trim().is_empty() {
        diagnostics.push(
            DotagentsDiagnostic::warning(
                DotagentsDiagnosticCode::MissingField,
                format!(
                    "agent profile `{id}` has no `description`; it will be advertised without one"
                ),
            )
            .with_source(source.clone()),
        );
    }

    let agent = DotagentsAgent {
        id,
        name,
        description,
        enabled,
        role,
        connection,
        capabilities,
        // `config.json` is applied separately by the caller, since it is a
        // sibling file with its own confinement and diagnostics.
        config: DotagentsAgentConfig::default(),
        body: parsed.body,
        extensions,
        source: source.clone(),
        fingerprint: fingerprint(text),
    };

    Ok(ParsedAgentDocument {
        agent: Some(agent),
        diagnostics,
    })
}

/// Interpret the `connection` frontmatter field of a profile.
///
/// The protocol may spell this as a scalar (`connection: internal`) or as a
/// nested mapping (`connection:\n  type: stdio\n  command: foo`).
fn parse_agent_connection(fields: &BTreeMap<String, Value>) -> DotagentsAgentConnection {
    match fields.get("connection") {
        Some(Value::Object(map)) => {
            let declared_type = map
                .get("type")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let command = map
                .get("command")
                .and_then(|value| value.as_str())
                .map(str::to_string);
            let connection_type = declared_type
                .as_deref()
                .map(DotagentsAgentConnectionType::parse)
                .unwrap_or(DotagentsAgentConnectionType::Internal);
            DotagentsAgentConnection {
                connection_type,
                declared_type,
                command,
            }
        }
        Some(Value::String(declared)) => DotagentsAgentConnection {
            connection_type: DotagentsAgentConnectionType::parse(declared),
            declared_type: Some(declared.clone()),
            command: None,
        },
        _ => match fields.get("connectionType") {
            Some(Value::String(declared)) => DotagentsAgentConnection {
                connection_type: DotagentsAgentConnectionType::parse(declared),
                declared_type: Some(declared.clone()),
                command: None,
            },
            _ => DotagentsAgentConnection::default(),
        },
    }
}

/// Parse an agent's adjacent `config.json` into supported settings.
///
/// Unknown fields are retained in [`DotagentsAgentConfig::extensions`] for
/// inspection and diagnostics rather than silently dropped.
pub fn parse_agent_config(
    text: &str,
    source: DotagentsSource,
) -> Result<DotagentsAgentConfig, Box<DotagentsDiagnostic>> {
    let object = parse_json_object(text, &source)?;
    let what = "agent `config.json`";

    let model_preset =
        optional_string(&object, "model").or_else(|| optional_string(&object, "modelPreset"));

    let tools = if object.contains_key("tools") {
        Some(string_list(&object, "tools", &source, what)?)
    } else {
        None
    };

    let mcp_servers = if object.contains_key("mcpServers") {
        Some(string_list(&object, "mcpServers", &source, what)?)
    } else if object.contains_key("mcp") {
        Some(string_list(&object, "mcp", &source, what)?)
    } else {
        None
    };

    Ok(DotagentsAgentConfig {
        model_preset,
        tools,
        mcp_servers,
        extensions: json_extensions(&object, AGENT_CONFIG_KEYS),
    })
}

// ---------------------------------------------------------------------------
// `tasks/<id>/task.md`
// ---------------------------------------------------------------------------

/// Frontmatter keys recognized on a repeat task document.
/// Frontmatter keys recognized on a repeat task document.
///
/// Spellings must match the exact keys the frontmatter accessors look up
/// (`runOnStartup`, `intervalMinutes`, `profileId`); [`partition_fields`]
/// re-keys case-insensitive matches to these canonical spellings.
const TASK_KEYS: &[&str] = &[
    "id",
    "name",
    "kind",
    "enabled",
    "runOnStartup",
    "intervalMinutes",
    "profileId",
];

/// Parse a `task.md` repeat task document.
///
/// The prompt body is required: a task with an empty body cannot execute
/// anything, so it is rejected with an actionable diagnostic. `intervalMinutes`
/// is validated as a positive integer when present.
pub fn parse_task_document(
    text: &str,
    entry_id: &str,
    source: DotagentsSource,
) -> Result<DotagentsTask, Box<DotagentsDiagnostic>> {
    let parsed =
        parse_frontmatter_markdown(text).map_err(|error| error.with_source(source.clone()))?;
    let (recognized, extensions) = partition_fields(&parsed.fields, TASK_KEYS);

    let id = field_string(&recognized, "id").unwrap_or_else(|| entry_id.to_string());
    let name = field_string(&recognized, "name").unwrap_or_else(|| id.clone());

    let kind = match field_string(&recognized, "kind") {
        Some(kind) => DotagentsTaskKind::parse(&kind),
        None => DotagentsTaskKind::Task,
    };

    let enabled = field_bool(&recognized, "enabled").unwrap_or(true);
    let run_on_startup = field_bool(&recognized, "runOnStartup").unwrap_or(false);
    let profile_id = field_string(&recognized, "profileId");

    let interval_minutes = match field_u64(&recognized, "intervalMinutes") {
        Some(0) => {
            return Err(Box::new(
                DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::ParseError,
                    format!("task `{id}` has `intervalMinutes: 0`; the interval must be positive"),
                )
                .with_source(source),
            ));
        }
        other => other,
    };

    let prompt = parsed.body.trim().to_string();
    if prompt.is_empty() {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::MissingField,
                format!("task `{id}` has an empty prompt body"),
            )
            .with_source(source),
        ));
    }

    Ok(DotagentsTask {
        id,
        name,
        kind,
        enabled,
        run_on_startup,
        interval_minutes,
        profile_id,
        prompt,
        extensions,
        source: source.clone(),
        fingerprint: fingerprint(text),
    })
}

// ---------------------------------------------------------------------------
// `memories/<id>.md`
// ---------------------------------------------------------------------------

/// Frontmatter keys recognized on a memory document.
const MEMORY_KEYS: &[&str] = &["id", "title", "tags", "importance", "content", "enabled"];

/// Parse a `memories/*.md` document.
///
/// `entry_id` is the file stem, used as the stable ID unless the frontmatter
/// provides an explicit `id`. The body may be empty when explicit `content`
/// metadata is provided, since the importer falls back to it.
pub fn parse_memory_document(
    text: &str,
    entry_id: &str,
    source: DotagentsSource,
) -> Result<DotagentsMemory, Box<DotagentsDiagnostic>> {
    let parsed =
        parse_frontmatter_markdown(text).map_err(|error| error.with_source(source.clone()))?;
    let (recognized, extensions) = partition_fields(&parsed.fields, MEMORY_KEYS);

    let id = field_string(&recognized, "id").unwrap_or_else(|| entry_id.to_string());
    let title = field_string(&recognized, "title");
    let tags = field_list(&recognized, "tags").unwrap_or_default();
    let importance = field_string(&recognized, "importance");
    let content = field_string(&recognized, "content");
    let enabled = field_bool(&recognized, "enabled").unwrap_or(true);

    let body = parsed.body;
    if body.trim().is_empty() && content.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::MissingField,
                format!("memory `{id}` has neither a Markdown body nor `content` metadata"),
            )
            .with_source(source),
        ));
    }

    Ok(DotagentsMemory {
        id,
        title,
        tags,
        importance,
        content,
        body,
        enabled,
        extensions,
        source: source.clone(),
        fingerprint: fingerprint(text),
    })
}

// ---------------------------------------------------------------------------
// `skills/<id>/skill.md` and `skills/<id>/SKILL.md`
// ---------------------------------------------------------------------------

/// Frontmatter keys recognized on a skill document.
///
/// Spellings must match what [`partition_fields`] re-keys to, so the
/// frontmatter accessors find them regardless of the document's casing.
const SKILL_KEYS: &[&str] = &["id", "name", "description", "enabled"];

/// Parse a protocol skill document (`skill.md` or `SKILL.md`).
///
/// `entry_id` is the containing directory name, used as the stable ID unless
/// the frontmatter provides an explicit `id`. Both spellings parse
/// identically so existing uppercase skills remain discoverable after
/// protocol support is enabled.
pub fn parse_skill_document(
    text: &str,
    entry_id: &str,
    source: DotagentsSource,
) -> Result<DotagentsSkill, Box<DotagentsDiagnostic>> {
    let parsed =
        parse_frontmatter_markdown(text).map_err(|error| error.with_source(source.clone()))?;
    let (recognized, extensions) = partition_fields(&parsed.fields, SKILL_KEYS);

    let id = field_string(&recognized, "id").unwrap_or_else(|| entry_id.to_string());
    let name = field_string(&recognized, "name").unwrap_or_else(|| id.clone());
    let description = field_string(&recognized, "description").unwrap_or_default();
    let enabled = field_bool(&recognized, "enabled").unwrap_or(true);

    if description.trim().is_empty() {
        return Err(Box::new(
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::MissingField,
                format!("skill `{id}` requires a `description` for discovery"),
            )
            .with_source(source),
        ));
    }

    Ok(DotagentsSkill {
        id,
        name,
        description,
        enabled,
        body: parsed.body,
        extensions,
        source: source.clone(),
        fingerprint: fingerprint(text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dotagents::DotagentsManifest;
    use crate::dotagents::layer::DotagentsLayer;

    fn workspace_source(path: &str) -> DotagentsSource {
        DotagentsSource::singleton(DotagentsLayer::Workspace, path)
    }

    fn entry_source(path: &str, id: &str) -> DotagentsSource {
        DotagentsSource::entry(DotagentsLayer::Workspace, path, id)
    }

    // ---- Markdown singletons: agents.md / system-prompt.md ----

    #[test]
    fn minimal_prompt_document_parses_body() {
        let prompt = parse_prompt_document(
            "# Instructions\n\nBe concise.\n",
            workspace_source("agents.md"),
        )
        .unwrap();
        assert!(prompt.metadata.is_empty());
        assert_eq!(prompt.body, "# Instructions\n\nBe concise.\n");
        assert!(!prompt.is_empty());
    }

    #[test]
    fn full_prompt_document_excludes_frontmatter_from_body() {
        let doc = "---\nname: agents\nfuture: keep-me\n---\n# Instructions\n\nBody text.\n";
        let prompt = parse_prompt_document(doc, workspace_source("agents.md")).unwrap();
        // Frontmatter is retained as metadata but never leaks into the body.
        assert_eq!(
            field_string(&prompt.metadata, "future").as_deref(),
            Some("keep-me")
        );
        assert!(!prompt.body.contains("---"));
        assert!(!prompt.body.contains("future: keep-me"));
        assert!(prompt.body.contains("# Instructions"));
    }

    #[test]
    fn empty_prompt_document_is_reported_empty() {
        let prompt = parse_prompt_document(
            "---\nname: x\n---\n\n",
            workspace_source("system-prompt.md"),
        )
        .unwrap();
        assert!(prompt.is_empty());
    }

    #[test]
    fn prompt_fingerprint_is_stable_and_content_sensitive() {
        let source = || workspace_source("agents.md");
        let a = parse_prompt_document("body", source()).unwrap();
        let b = parse_prompt_document("body", source()).unwrap();
        let c = parse_prompt_document("other", source()).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.fingerprint, c.fingerprint);
    }

    #[test]
    fn malformed_prompt_reports_source() {
        let err =
            parse_prompt_document("---\nbad line\n", workspace_source("agents.md")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
        assert_eq!(
            err.source.as_ref().unwrap().lexical_path.to_str(),
            Some("agents.md")
        );
    }

    // ---- mcp.json ----

    #[test]
    fn minimal_mcp_document_infers_stdio_from_command() {
        let doc = r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","server-fs"]}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(parsed.diagnostics.is_empty());
        let server = parsed.servers.get("fs").unwrap();
        assert_eq!(server.transport, DotagentsMcpTransport::Stdio);
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, vec!["-y", "server-fs"]);
        assert!(server.enabled);
    }

    #[test]
    fn minimal_mcp_document_infers_http_from_url() {
        let doc = r#"{"mcpServers":{"web":{"url":"https://example.test/mcp"}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        let server = parsed.servers.get("web").unwrap();
        assert_eq!(server.transport, DotagentsMcpTransport::StreamableHttp);
        assert_eq!(server.url.as_deref(), Some("https://example.test/mcp"));
    }

    #[test]
    fn full_mcp_document_preserves_env_headers_and_extensions() {
        let doc = r#"{
            "mcpServers": {
                "fs": {
                    "transport": "stdio",
                    "command": "node",
                    "args": ["server.js"],
                    "env": {"TOKEN": "${TOKEN}"},
                    "enabled": true,
                    "futureField": 7
                },
                "web": {
                    "transport": "streamable-http",
                    "url": "https://example.test/mcp",
                    "headers": {"Authorization": "Bearer ${TOKEN}"}
                }
            }
        }"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.servers.len(), 2);

        let fs = parsed.servers.get("fs").unwrap();
        assert_eq!(fs.declared_transport.as_deref(), Some("stdio"));
        assert_eq!(fs.env.get("TOKEN").map(String::as_str), Some("${TOKEN}"));
        assert!(fs.extensions.contains_key("futureField"));

        let web = parsed.servers.get("web").unwrap();
        assert_eq!(web.transport, DotagentsMcpTransport::StreamableHttp);
        assert_eq!(
            web.headers.get("Authorization").map(String::as_str),
            Some("Bearer ${TOKEN}")
        );
    }

    #[test]
    fn mcp_document_without_servers_key_is_empty_not_error() {
        let parsed = parse_mcp_document(r#"{"other":1}"#, workspace_source("mcp.json")).unwrap();
        assert!(parsed.servers.is_empty());
        assert!(parsed.diagnostics.is_empty());
    }

    #[test]
    fn mcp_conflicting_command_and_url_is_rejected() {
        let doc = r#"{"mcpServers":{"bad":{"command":"x","url":"https://e.test"}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(!parsed.servers.contains_key("bad"));
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].code,
            DotagentsDiagnosticCode::ConflictingFields
        );
        assert_eq!(
            parsed.diagnostics[0]
                .source
                .as_ref()
                .unwrap()
                .entry_id
                .as_deref(),
            Some("bad")
        );
    }

    #[test]
    fn mcp_websocket_transport_is_rejected_with_diagnostic() {
        let doc = r#"{"mcpServers":{"ws":{"transport":"websocket","url":"wss://e.test"}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(parsed.servers.is_empty());
        assert_eq!(
            parsed.diagnostics[0].code,
            DotagentsDiagnosticCode::UnsupportedTransport
        );
    }

    #[test]
    fn mcp_unknown_transport_is_rejected_with_diagnostic() {
        let doc = r#"{"mcpServers":{"x":{"transport":"carrier-pigeon","command":"c"}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(parsed.servers.is_empty());
        assert!(parsed.diagnostics[0].message.contains("carrier-pigeon"));
    }

    #[test]
    fn mcp_missing_stdio_command_is_rejected() {
        let doc = r#"{"mcpServers":{"x":{"transport":"stdio"}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(parsed.servers.is_empty());
        assert_eq!(
            parsed.diagnostics[0].code,
            DotagentsDiagnosticCode::MissingField
        );
    }

    #[test]
    fn mcp_missing_http_url_is_rejected() {
        let doc = r#"{"mcpServers":{"x":{"transport":"streamable-http"}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(parsed.servers.is_empty());
        assert_eq!(
            parsed.diagnostics[0].code,
            DotagentsDiagnosticCode::MissingField
        );
    }

    #[test]
    fn mcp_omitted_transport_with_neither_command_nor_url_is_rejected() {
        let doc = r#"{"mcpServers":{"ambiguous":{"enabled":true}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        assert!(parsed.servers.is_empty());
        assert_eq!(parsed.diagnostics.len(), 1);
        let diag = &parsed.diagnostics[0];
        assert_eq!(diag.code, DotagentsDiagnosticCode::MissingField);
        assert!(diag.message.contains("neither `command` nor `url`"));
        // Source-aware: the diagnostic identifies the layer, file, and entry.
        let source = diag.source.as_ref().expect("diagnostic carries a source");
        assert_eq!(source.layer, DotagentsLayer::Workspace);
        assert!(source.path().ends_with("mcp.json"));
        assert_eq!(source.entry_id.as_deref(), Some("ambiguous"));
    }

    #[test]
    fn mcp_invalid_sibling_is_isolated_from_valid_entry() {
        let doc = r#"{"mcpServers":{
            "good":{"command":"npx"},
            "bad":{"command":"x","url":"https://e.test"}
        }}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        // A malformed sibling must not hide the valid entry.
        assert!(parsed.servers.contains_key("good"));
        assert!(!parsed.servers.contains_key("bad"));
        assert_eq!(parsed.diagnostics.len(), 1);
    }

    #[test]
    fn mcp_malformed_json_is_an_error() {
        let err = parse_mcp_document("{not json", workspace_source("mcp.json")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
    }

    #[test]
    fn mcp_non_object_top_level_is_an_error() {
        let err = parse_mcp_document("[]", workspace_source("mcp.json")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
    }

    #[test]
    fn mcp_disabled_entry_is_retained_but_not_supported() {
        let doc = r#"{"mcpServers":{"fs":{"command":"npx","enabled":false}}}"#;
        let parsed = parse_mcp_document(doc, workspace_source("mcp.json")).unwrap();
        let server = parsed.servers.get("fs").unwrap();
        assert!(!server.enabled);
        assert!(!server.is_supported());
    }

    // ---- models.json ----

    #[test]
    fn minimal_model_preset_parses() {
        let doc = r#"{"models":{"fast":{"provider":"anthropic","model":"claude"}}}"#;
        let parsed = parse_models_document(doc, workspace_source("models.json")).unwrap();
        assert!(parsed.diagnostics.is_empty());
        let preset = parsed.presets.get("fast").unwrap();
        assert_eq!(preset.provider, "anthropic");
        assert_eq!(preset.model, "claude");
        assert!(preset.credential.is_none());
        assert!(preset.parameters.is_empty());
    }

    #[test]
    fn top_level_model_presets_are_accepted() {
        // The draft protocol also allows presets directly at the top level.
        let doc = r#"{"fast":{"provider":"openai","model":"gpt"}}"#;
        let parsed = parse_models_document(doc, workspace_source("models.json")).unwrap();
        assert!(parsed.presets.contains_key("fast"));
    }

    #[test]
    fn mixed_case_api_key_is_a_known_model_field() {
        let doc = r#"{"models":{"fast":{"provider":"p","model":"m","apiKey":"secret"}}}"#;
        let parsed = parse_models_document(doc, workspace_source("models.json")).unwrap();
        let preset = parsed.presets.get("fast").unwrap();
        assert_eq!(preset.credential.as_deref(), Some("secret"));
        assert!(!preset.extensions.contains_key("apiKey"));
    }

    #[test]
    fn full_model_preset_parses_credential_and_parameters() {
        let doc = r#"{"models":{"fast":{
            "provider":"anthropic",
            "model":"claude",
            "credential":"${ANTHROPIC_API_KEY}",
            "parameters":{"temperature":0.2,"maxTokens":1024},
            "futureField":"kept"
        }}}"#;
        let parsed = parse_models_document(doc, workspace_source("models.json")).unwrap();
        let preset = parsed.presets.get("fast").unwrap();
        assert_eq!(preset.credential.as_deref(), Some("${ANTHROPIC_API_KEY}"));
        assert_eq!(
            preset.parameters.get("temperature").and_then(Value::as_f64),
            Some(0.2)
        );
        assert_eq!(
            preset.parameters.get("maxTokens").and_then(Value::as_u64),
            Some(1024)
        );
        assert!(preset.extensions.contains_key("futureField"));
    }

    #[test]
    fn model_preset_missing_provider_is_rejected() {
        let doc = r#"{"models":{"bad":{"model":"claude"}}}"#;
        let parsed = parse_models_document(doc, workspace_source("models.json")).unwrap();
        assert!(parsed.presets.is_empty());
        assert_eq!(
            parsed.diagnostics[0].code,
            DotagentsDiagnosticCode::MissingField
        );
    }

    #[test]
    fn model_preset_missing_model_is_rejected() {
        let doc = r#"{"models":{"bad":{"provider":"anthropic"}}}"#;
        let parsed = parse_models_document(doc, workspace_source("models.json")).unwrap();
        assert!(parsed.presets.is_empty());
        assert_eq!(
            parsed.diagnostics[0].code,
            DotagentsDiagnosticCode::MissingField
        );
    }

    #[test]
    fn invalid_model_sibling_is_isolated() {
        let doc = r#"{"models":{"good":{"provider":"p","model":"m"},"bad":{"model":"m"}}}"#;
        let parsed = parse_models_document(doc, workspace_source("models.json")).unwrap();
        assert!(parsed.presets.contains_key("good"));
        assert!(!parsed.presets.contains_key("bad"));
        assert_eq!(parsed.diagnostics.len(), 1);
    }

    #[test]
    fn model_malformed_json_is_an_error() {
        let err = parse_models_document("nope", workspace_source("models.json")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
    }

    // ---- agents/<id>/agent.md ----

    #[test]
    fn minimal_agent_profile_uses_directory_id() {
        let doc = "---\nname: Helper\ndescription: Helps out\n---\nYou are a helper.\n";
        let parsed = parse_agent_document(
            doc,
            "helper",
            entry_source("agents/helper/agent.md", "helper"),
        )
        .unwrap();
        let agent = parsed.agent.unwrap();
        assert_eq!(agent.id, "helper");
        assert_eq!(agent.name, "Helper");
        assert_eq!(agent.description, "Helps out");
        assert!(agent.enabled);
        assert_eq!(agent.role, DotagentsAgentRole::DelegationTarget);
        assert_eq!(
            agent.connection.connection_type,
            DotagentsAgentConnectionType::Internal
        );
        assert_eq!(agent.body, "You are a helper.\n");
    }

    #[test]
    fn full_agent_profile_parses_role_connection_and_capabilities() {
        let doc = "---\nid: custom-id\nname: Reviewer\ndescription: Reviews code\nenabled: true\n\
            role: delegation-target\ncapabilities: [read, write]\n\
            connection:\n  type: internal\nfuture: keep\n---\nBody.\n";
        let parsed =
            parse_agent_document(doc, "dir", entry_source("agents/dir/agent.md", "dir")).unwrap();
        let agent = parsed.agent.unwrap();
        assert_eq!(agent.id, "custom-id");
        assert_eq!(agent.capabilities, vec!["read", "write"]);
        assert!(agent.extensions.contains_key("future"));
    }

    #[test]
    fn agent_profile_with_scalar_connection_parses() {
        let doc = "---\ndescription: d\nconnection: stdio\n---\nBody.\n";
        let parsed =
            parse_agent_document(doc, "a", entry_source("agents/a/agent.md", "a")).unwrap();
        let agent = parsed.agent.unwrap();
        assert_eq!(
            agent.connection.connection_type,
            DotagentsAgentConnectionType::Stdio
        );
        assert_eq!(agent.connection.declared_type.as_deref(), Some("stdio"));
    }

    #[test]
    fn agent_profile_with_connection_type_only_is_unsupported() {
        let doc = "---\ndescription: d\nconnectionType: stdio\n---\nBody.\n";
        let parsed =
            parse_agent_document(doc, "a", entry_source("agents/a/agent.md", "a")).unwrap();
        let agent = parsed.agent.unwrap();
        assert_eq!(
            agent.connection.connection_type,
            DotagentsAgentConnectionType::Stdio
        );
        assert_eq!(agent.connection.declared_type.as_deref(), Some("stdio"));

        let mut manifest = DotagentsManifest::default();
        manifest.agents.insert(agent.id.clone(), agent);
        let plans = super::super::subagent::DotagentsSubAgentPlans::from_manifest(&manifest);
        let plan = plans.plans.get("a").unwrap();
        assert_eq!(
            plan.diagnostics[0].code,
            DotagentsDiagnosticCode::UnsupportedTransport
        );
    }

    #[test]
    fn agent_profile_disabled_is_retained() {
        let doc = "---\ndescription: d\nenabled: false\n---\nBody.\n";
        let parsed =
            parse_agent_document(doc, "a", entry_source("agents/a/agent.md", "a")).unwrap();
        assert!(!parsed.agent.unwrap().enabled);
    }

    #[test]
    fn agent_profile_without_description_warns_but_parses() {
        let doc = "---\nname: x\n---\nBody.\n";
        let parsed =
            parse_agent_document(doc, "a", entry_source("agents/a/agent.md", "a")).unwrap();
        assert!(parsed.agent.is_some());
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(
            parsed.diagnostics[0].code,
            DotagentsDiagnosticCode::MissingField
        );
    }

    #[test]
    fn agent_profile_without_frontmatter_is_body_only() {
        let parsed = parse_agent_document(
            "Just a prompt.\n",
            "a",
            entry_source("agents/a/agent.md", "a"),
        )
        .unwrap();
        let agent = parsed.agent.unwrap();
        assert_eq!(agent.id, "a");
        assert_eq!(agent.body, "Just a prompt.\n");
    }

    #[test]
    fn agent_config_minimal_and_full_parse() {
        let minimal = parse_agent_config(
            r#"{"model":"fast"}"#,
            entry_source("agents/a/config.json", "a"),
        )
        .unwrap();
        assert_eq!(minimal.model_preset.as_deref(), Some("fast"));
        assert!(minimal.tools.is_none());
        assert!(minimal.mcp_servers.is_none());

        let full = parse_agent_config(
            r#"{"model":"fast","tools":["read"],"mcpServers":["fs"],"other":1}"#,
            entry_source("agents/a/config.json", "a"),
        )
        .unwrap();
        assert_eq!(full.tools.as_deref(), Some(&["read".to_string()][..]));
        assert_eq!(full.mcp_servers.as_deref(), Some(&["fs".to_string()][..]));
        assert!(full.extensions.contains_key("other"));
    }

    #[test]
    fn agent_config_accepts_mcp_alias() {
        let config = parse_agent_config(
            r#"{"mcp":["fs"]}"#,
            entry_source("agents/a/config.json", "a"),
        )
        .unwrap();
        assert_eq!(config.mcp_servers.as_deref(), Some(&["fs".to_string()][..]));
    }

    #[test]
    fn agent_config_malformed_json_is_an_error() {
        let err =
            parse_agent_config("nope", entry_source("agents/a/config.json", "a")).unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
    }

    // ---- tasks/<id>/task.md ----

    #[test]
    fn minimal_task_parses_with_body_prompt() {
        let doc = "---\nkind: task\nintervalMinutes: 60\n---\nSummarize the repo.\n";
        let task = parse_task_document(
            doc,
            "digest",
            entry_source("tasks/digest/task.md", "digest"),
        )
        .unwrap();
        assert_eq!(task.id, "digest");
        assert_eq!(task.kind, DotagentsTaskKind::Task);
        assert_eq!(task.interval_minutes, Some(60));
        assert!(task.enabled);
        assert!(!task.run_on_startup);
        assert_eq!(task.prompt, "Summarize the repo.");
    }

    #[test]
    fn full_task_parses_all_fields() {
        let doc = "---\nid: digest\nname: Digest\nkind: task\nenabled: true\n\
            runOnStartup: true\nintervalMinutes: 30\nprofileId: helper\nfuture: keep\n---\nPrompt.\n";
        let task =
            parse_task_document(doc, "dir", entry_source("tasks/dir/task.md", "dir")).unwrap();
        assert_eq!(task.id, "digest");
        assert_eq!(task.name, "Digest");
        assert!(task.run_on_startup);
        assert_eq!(task.interval_minutes, Some(30));
        assert_eq!(task.profile_id.as_deref(), Some("helper"));
        assert!(task.extensions.contains_key("future"));
    }

    #[test]
    fn task_defaults_kind_to_task_when_absent() {
        let task = parse_task_document(
            "---\nintervalMinutes: 5\n---\nP",
            "t",
            entry_source("tasks/t/task.md", "t"),
        )
        .unwrap();
        assert_eq!(task.kind, DotagentsTaskKind::Task);
    }

    #[test]
    fn task_empty_prompt_is_rejected() {
        let err = parse_task_document(
            "---\nintervalMinutes: 5\n---\n\n",
            "t",
            entry_source("tasks/t/task.md", "t"),
        )
        .unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::MissingField);
    }

    #[test]
    fn task_zero_interval_is_rejected() {
        let err = parse_task_document(
            "---\nintervalMinutes: 0\n---\nP",
            "t",
            entry_source("tasks/t/task.md", "t"),
        )
        .unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::ParseError);
    }

    #[test]
    fn task_without_frontmatter_uses_directory_id() {
        let task =
            parse_task_document("Do it.", "t", entry_source("tasks/t/task.md", "t")).unwrap();
        assert_eq!(task.id, "t");
        assert_eq!(task.interval_minutes, None);
    }

    #[test]
    fn task_kind_other_is_retained() {
        let task = parse_task_document(
            "---\nkind: report\n---\nP",
            "t",
            entry_source("tasks/t/task.md", "t"),
        )
        .unwrap();
        assert_eq!(task.kind, DotagentsTaskKind::Other);
    }

    // ---- memories/<id>.md ----

    #[test]
    fn minimal_memory_parses_from_body() {
        let memory = parse_memory_document(
            "Remember the deploy checklist.\n",
            "deploy",
            entry_source("memories/deploy.md", "deploy"),
        )
        .unwrap();
        assert_eq!(memory.id, "deploy");
        assert_eq!(memory.body, "Remember the deploy checklist.\n");
        assert!(memory.tags.is_empty());
        assert!(memory.importance.is_none());
        assert!(memory.enabled);
    }

    #[test]
    fn full_memory_parses_metadata() {
        let doc = "---\nid: deploy\ntitle: Deploy\ntags: [ops, release]\n\
            importance: high\ncontent: Explicit content\nfuture: keep\n---\nBody.\n";
        let memory =
            parse_memory_document(doc, "dir", entry_source("memories/dir.md", "dir")).unwrap();
        assert_eq!(memory.id, "deploy");
        assert_eq!(memory.title.as_deref(), Some("Deploy"));
        assert_eq!(memory.tags, vec!["ops", "release"]);
        assert_eq!(memory.importance.as_deref(), Some("high"));
        assert_eq!(memory.content.as_deref(), Some("Explicit content"));
        assert!(memory.extensions.contains_key("future"));
    }

    #[test]
    fn memory_with_content_only_and_no_body_parses() {
        let doc = "---\ncontent: From metadata\n---\n";
        let memory = parse_memory_document(doc, "m", entry_source("memories/m.md", "m")).unwrap();
        assert_eq!(memory.content.as_deref(), Some("From metadata"));
        assert!(memory.body.trim().is_empty());
    }

    #[test]
    fn memory_without_body_or_content_is_rejected() {
        let err = parse_memory_document(
            "---\ntitle: empty\n---\n\n",
            "m",
            entry_source("memories/m.md", "m"),
        )
        .unwrap_err();
        assert_eq!(err.code, DotagentsDiagnosticCode::MissingField);
    }

    #[test]
    fn memory_disabled_is_retained() {
        let memory = parse_memory_document(
            "---\nenabled: false\n---\nBody.\n",
            "m",
            entry_source("memories/m.md", "m"),
        )
        .unwrap();
        assert!(!memory.enabled);
    }

    #[test]
    fn memory_csv_tags_are_accepted() {
        let memory = parse_memory_document(
            "---\ntags: one, two\n---\nBody.\n",
            "m",
            entry_source("memories/m.md", "m"),
        )
        .unwrap();
        assert_eq!(memory.tags, vec!["one", "two"]);
    }
}
