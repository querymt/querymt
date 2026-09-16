//! Runtime adaptation of protocol MCP entries.
//!
//! Protocol [`DotagentsMcpServer`] values are converted into QueryMT's
//! existing [`McpServerConfig`] variants so the runtime keeps using its normal
//! process-based stdio transport and RMCP-backed streamable HTTP transport.
//! No protocol-specific MCP client stack is introduced here.
//!
//! Conversion is pure: it never starts servers. Disabled entries are skipped
//! without diagnostics (they are intentionally inactive), and unsupported
//! transports are reported as diagnostics instead of being converted.
//!
//! Secret handling: `${VAR}` / `${VAR:-default}` environment references in MCP
//! environment and header values are interpolated **at activation time** using
//! QueryMT's existing rules (`[crate::config::interpolate_env_vars]`). The
//! manifest keeps unresolved values, diagnostics name missing references
//! without resolved values, and [`redacted_view`](DotagentsMcpServer::redacted_view)
//! / [`redacted_view`](DotagentsModelPreset::redacted_view) expose public/debug
//! representations with secret-bearing values masked.

use super::diagnostics::{DotagentsDiagnostic, DotagentsDiagnosticCode};
use super::manifest::{
    DotagentsManifest, DotagentsMcpServer, DotagentsMcpTransport, DotagentsModelPreset,
};
use crate::config::McpServerConfig;
use std::collections::{BTreeMap, HashMap};

/// Outcome of converting the resolved manifest's MCP entries.
///
/// Diagnostics cover unsupported transports and unresolved environment
/// references; affected servers are simply not activated.
#[derive(Debug, Default)]
pub struct DotagentsMcpPlan {
    /// Servers to attach through the existing MCP lifecycle, ordered by name.
    pub servers: Vec<McpServerConfig>,
    /// Diagnostics for entries that could not be converted.
    pub diagnostics: Vec<DotagentsDiagnostic>,
}

impl DotagentsMcpPlan {
    /// Convert every enabled, supported MCP entry in `manifest`.
    pub fn from_manifest(manifest: &DotagentsManifest) -> Self {
        Self::from_manifest_with_workspace_stdio_approval(manifest, false)
    }

    /// Convert a manifest after the host explicitly approves workspace stdio MCP servers.
    pub fn from_manifest_with_workspace_stdio_approval(
        manifest: &DotagentsManifest,
        workspace_stdio_approved: bool,
    ) -> Self {
        let mut plan = Self::default();
        for server in manifest.mcp_servers.values() {
            match convert_server_with_diagnostics(server, workspace_stdio_approved) {
                Ok((Some(config), diagnostics)) => {
                    plan.servers.push(config);
                    plan.diagnostics.extend(diagnostics);
                }
                Ok((None, diagnostics)) => plan.diagnostics.extend(diagnostics),
                Err(diagnostic) => plan.diagnostics.push(diagnostic),
            }
        }
        plan
    }
}

/// Interpolate QueryMT `${VAR}` / `${VAR:-default}` references in a protocol
/// string value at activation time.
///
/// The manifest keeps unresolved values. Failures produce actionable
/// `MissingEnvironment` diagnostics that name the unresolved references (the
/// underlying error contains variable names only) without any resolved secret
/// values.
pub(crate) fn interpolate_activation_string(
    context: &str,
    value: &str,
    source: &super::layer::DotagentsSource,
) -> Result<String, DotagentsDiagnostic> {
    crate::config::interpolate_env_vars(value).map_err(|err| {
        DotagentsDiagnostic::error(
            DotagentsDiagnosticCode::MissingEnvironment,
            format!("{context}: {err}"),
        )
        .with_source(source.clone())
    })
}

/// Interpolate one secret-bearing map (MCP env or HTTP headers) in place.
fn interpolate_map(
    context: &str,
    values: &BTreeMap<String, String>,
    source: &super::layer::DotagentsSource,
) -> Result<HashMap<String, String>, DotagentsDiagnostic> {
    let mut resolved = HashMap::with_capacity(values.len());
    for (key, raw) in values {
        let value = interpolate_activation_string(&format!("{context} `{key}`"), raw, source)?;
        resolved.insert(key.clone(), value);
    }
    Ok(resolved)
}

/// Convert a single protocol MCP entry into the existing MCP configuration.
///
/// Returns `Ok(None)` for disabled entries, which callers should simply skip.
/// Environment and header values are interpolated at this activation
/// boundary; a missing environment variable prevents that server from being
/// activated and yields an actionable diagnostic.
pub fn convert_server(
    server: &DotagentsMcpServer,
) -> Result<Option<McpServerConfig>, DotagentsDiagnostic> {
    convert_server_with_workspace_stdio_approval(server, false)
}

/// Convert a server after the host explicitly decides whether to trust workspace stdio execution.
pub fn convert_server_with_workspace_stdio_approval(
    server: &DotagentsMcpServer,
    workspace_stdio_approved: bool,
) -> Result<Option<McpServerConfig>, DotagentsDiagnostic> {
    convert_server_with_diagnostics(server, workspace_stdio_approved).map(|(config, _)| config)
}

pub(crate) fn convert_server_with_diagnostics(
    server: &DotagentsMcpServer,
    workspace_stdio_approved: bool,
) -> Result<(Option<McpServerConfig>, Vec<DotagentsDiagnostic>), DotagentsDiagnostic> {
    if !server.enabled {
        return Ok((None, Vec::new()));
    }
    match server.transport {
        DotagentsMcpTransport::Stdio => {
            if server.source.layer == super::layer::DotagentsLayer::Workspace
                && !workspace_stdio_approved
            {
                return Err(DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::UnsafePolicy,
                    format!(
                        "workspace MCP server `{}` uses `stdio` and requires explicit host approval",
                        server.name
                    ),
                )
                .with_source(server.source.clone()));
            }
            let Some(command) = server.command.clone() else {
                return Err(DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::MissingField,
                    format!("MCP server `{}` uses `stdio` transport and requires a `command`", server.name),
                )
                .with_source(server.source.clone()));
            };
            let env = interpolate_map(
                &format!("MCP server `{}` environment variable", server.name),
                &server.env,
                &server.source,
            )?;
            Ok((
                Some(McpServerConfig::Stdio {
                    name: server.name.clone(),
                    command,
                    args: server.args.clone(),
                    env,
                }),
                Vec::new(),
            ))
        }
        DotagentsMcpTransport::StreamableHttp => {
            let Some(url) = server.url.clone() else {
                return Err(DotagentsDiagnostic::error(
                    DotagentsDiagnosticCode::MissingField,
                    format!(
                        "MCP server `{}` uses `streamable-http` transport and requires a `url`",
                        server.name
                    ),
                )
                .with_source(server.source.clone()));
            };
            let headers = interpolate_map(
                &format!("MCP server `{}` header", server.name),
                &server.headers,
                &server.source,
            )?;
            let diagnostics = insecure_http_headers_warning(server, &url)
                .into_iter()
                .collect::<Vec<_>>();
            if !diagnostics.is_empty() {
                return Ok((None, diagnostics));
            }
            Ok((
                Some(McpServerConfig::Http {
                    name: server.name.clone(),
                    url,
                    headers,
                }),
                diagnostics,
            ))
        }
        DotagentsMcpTransport::WebSocket | DotagentsMcpTransport::Unknown => {
            Err(DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::UnsupportedTransport,
                format!(
                    "MCP server `{}` requests `{}` transport, which the current QueryMT MCP runtime cannot represent",
                    server.name, server.transport
                ),
            )
            .with_source(server.source.clone()))
        }
    }
}

fn insecure_http_headers_warning(
    server: &DotagentsMcpServer,
    url: &str,
) -> Option<DotagentsDiagnostic> {
    if server.headers.is_empty() || !url.to_ascii_lowercase().starts_with("http://") {
        return None;
    }
    let authority = url[7..].split('/').next().unwrap_or_default();
    let host_and_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(bracketed) = host_and_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or_default()
    } else {
        host_and_port.split(':').next().unwrap_or_default()
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    (!loopback).then(|| {
        DotagentsDiagnostic::error(
            DotagentsDiagnosticCode::Other,
            format!(
                "MCP server `{}` cannot send interpolated headers over non-loopback HTTP; use HTTPS to protect credentials",
                server.name
            ),
        )
        .with_source(server.source.clone())
    })
}

/// Placeholder for secret-bearing values in redacted views.
///
/// It never stores the secret: every format renders a fixed mask so resolved
/// values cannot leak through `Debug`, `Display`, or serialization of the
/// view types.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RedactedValue;

impl std::fmt::Debug for RedactedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl std::fmt::Display for RedactedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// Redacted public/debug view of a [`DotagentsMcpServer`].
///
/// Environment and header values are masked; identifiers, transport, command,
/// arguments, and source provenance remain inspectable so configuration
/// problems can still be corrected without exposing secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsMcpServerView {
    pub name: String,
    pub transport: DotagentsMcpTransport,
    pub declared_transport: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, RedactedValue>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, RedactedValue>,
    pub enabled: bool,
    pub source: super::layer::DotagentsSource,
}

impl std::fmt::Display for DotagentsMcpServerView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP server `{}` ({}, {}, from {})",
            self.name,
            self.transport,
            if self.enabled { "enabled" } else { "disabled" },
            self.source
        )?;
        if let Some(command) = &self.command {
            write!(f, " command={command}")?;
        }
        if let Some(url) = &self.url {
            write!(f, " url={url}")?;
        }
        write_masked_map(f, "env", self.env.keys())?;
        write_masked_map(f, "headers", self.headers.keys())
    }
}

/// Render `name={key: ***, key2: ***}` for a secret-bearing map in a redacted
/// view: keys stay inspectable, values are always the fixed mask.
fn write_masked_map<'a, I>(f: &mut std::fmt::Formatter<'_>, name: &str, keys: I) -> std::fmt::Result
where
    I: IntoIterator<Item = &'a String>,
{
    write!(f, " {name}={{")?;
    for (i, key) in keys.into_iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{key}: ***")?;
    }
    write!(f, "}}")
}

/// Redacted public/debug view of a [`DotagentsModelPreset`].
///
/// The credential and all provider-parameter values are masked; only the
/// parameter names remain visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotagentsModelPresetView {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub credential: Option<RedactedValue>,
    pub parameter_keys: Vec<String>,
    pub source: super::layer::DotagentsSource,
}

impl std::fmt::Display for DotagentsModelPresetView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "model preset `{}` ({}/{}, from {})",
            self.name, self.provider, self.model, self.source
        )?;
        if self.credential.is_some() {
            write!(f, " credential=***")?;
        }
        write_masked_map(f, "parameters", self.parameter_keys.iter())
    }
}

impl DotagentsMcpServer {
    /// Redacted public/debug representation with env/header values masked.
    pub fn redacted_view(&self) -> DotagentsMcpServerView {
        DotagentsMcpServerView {
            name: self.name.clone(),
            transport: self.transport,
            declared_transport: self.declared_transport.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self
                .env
                .keys()
                .map(|k| (k.clone(), RedactedValue))
                .collect(),
            url: self.url.clone(),
            headers: self
                .headers
                .keys()
                .map(|k| (k.clone(), RedactedValue))
                .collect(),
            enabled: self.enabled,
            source: self.source.clone(),
        }
    }
}

impl DotagentsModelPreset {
    /// Redacted public/debug representation with credential and parameter
    /// values masked.
    pub fn redacted_view(&self) -> DotagentsModelPresetView {
        DotagentsModelPresetView {
            name: self.name.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            credential: self.credential.as_ref().map(|_| RedactedValue),
            parameter_keys: self.parameters.keys().cloned().collect(),
            source: self.source.clone(),
        }
    }
}

/// Neutral LLM overlay produced from a selected protocol model preset.
///
/// The overlay records only what the preset represents. Applying it onto
/// existing [`querymt::LLMParams`] never drops unrelated base settings, and
/// `models.json` never installs providers: provider availability is validated
/// separately against the plugin registry (see [`validate_preset_provider`]).
#[derive(Debug, Clone, PartialEq)]
pub struct DotagentsLlmOverlay {
    pub provider: String,
    pub model: String,
    /// Credential with `${VAR}` references resolved at selection time.
    pub api_key: Option<String>,
    /// Provider parameters from the preset. Recognized keys are applied to
    /// typed LLM fields by [`DotagentsLlmOverlay::apply_to`]; everything else
    /// is added to `LLMParams::custom` without removing existing entries.
    pub parameters: BTreeMap<String, serde_json::Value>,
    /// Provenance of the selected preset.
    pub source: super::layer::DotagentsSource,
}

fn unknown_preset_diagnostic(preset_name: &str) -> DotagentsDiagnostic {
    DotagentsDiagnostic::error(
        DotagentsDiagnosticCode::UnknownPreset,
        format!("model preset `{preset_name}` is not defined by any enabled protocol layer"),
    )
}

/// Select a named preset from the resolved manifest and convert it into an
/// LLM overlay.
///
/// Fails with `UnknownPreset` when the name is not defined and with
/// `MissingEnvironment` when the credential's environment reference cannot be
/// resolved. Provider and model presence is guaranteed by parsing.
pub fn select_model_overlay(
    manifest: &DotagentsManifest,
    preset_name: &str,
) -> Result<DotagentsLlmOverlay, DotagentsDiagnostic> {
    let preset = manifest
        .model_preset(preset_name)
        .ok_or_else(|| unknown_preset_diagnostic(preset_name))?;
    preset_to_overlay(preset)
}

fn preset_to_overlay(
    preset: &DotagentsModelPreset,
) -> Result<DotagentsLlmOverlay, DotagentsDiagnostic> {
    let api_key = match &preset.credential {
        Some(credential) => Some(interpolate_activation_string(
            &format!("model preset `{}` credential", preset.name),
            credential,
            &preset.source,
        )?),
        None => None,
    };
    Ok(DotagentsLlmOverlay {
        provider: preset.provider.clone(),
        model: preset.model.clone(),
        api_key,
        parameters: preset.parameters.clone(),
        source: preset.source.clone(),
    })
}

/// Validate that the selected preset's provider is available in the plugin
/// registry.
///
/// `models.json` never installs providers and never replaces profile provider
/// locks; selecting a preset whose provider cannot be resolved fails with an
/// actionable diagnostic instead.
pub async fn validate_preset_provider(
    registry: &querymt::plugin::host::PluginRegistry,
    manifest: &DotagentsManifest,
    preset_name: &str,
) -> Result<(), DotagentsDiagnostic> {
    let preset = manifest
        .model_preset(preset_name)
        .ok_or_else(|| unknown_preset_diagnostic(preset_name))?;
    if registry.get(&preset.provider).await.is_none() {
        return Err(DotagentsDiagnostic::error(
            DotagentsDiagnosticCode::UnresolvedReference,
            format!(
                "model preset `{preset_name}` requires provider `{}`, which is not available in the plugin registry; \
                 install or configure the provider before selecting this preset",
                preset.provider
            ),
        )
        .with_source(preset.source.clone()));
    }
    Ok(())
}

/// Resolve, validate, and apply a selected preset in one step.
///
/// This is the runtime entry point for model preset activation. It composes the
/// three steps so a caller cannot accidentally apply an overlay before checking
/// that its provider is available:
///
/// 1. resolve the named preset into an overlay (unknown preset, missing
///    credential environment reference);
/// 2. validate the provider against the injected plugin registry (unavailable
///    provider);
/// 3. apply the overlay to `params`.
///
/// If any step fails, `params` is left exactly as it was. `models.json` never
/// installs providers and never replaces profile provider locks.
pub async fn apply_selected_model_preset(
    registry: &querymt::plugin::host::PluginRegistry,
    manifest: &DotagentsManifest,
    preset_name: &str,
    params: &mut querymt::LLMParams,
) -> Result<(), DotagentsDiagnostic> {
    let overlay = select_model_overlay(manifest, preset_name)?;
    validate_preset_provider(registry, manifest, preset_name).await?;
    // `apply_to` validates every parameter before mutating `params`.
    overlay.apply_to(params)
}

impl DotagentsLlmOverlay {
    /// Overlay the preset onto existing LLM params.
    ///
    /// Only fields the preset represents are replaced; unrelated base
    /// settings (name, system parts, existing custom parameters, ...) are
    /// preserved. Recognized provider parameters map onto typed LLM fields;
    /// invalid values fail with a `ParseError` naming the parameter.
    ///
    /// The overlay is transactional: every parameter is validated before any
    /// field is written, so a rejected preset never leaves a partial overlay
    /// behind.
    pub fn apply_to(&self, params: &mut querymt::LLMParams) -> Result<(), DotagentsDiagnostic> {
        let invalid = |key: &str, value: &serde_json::Value, expected: &str| {
            DotagentsDiagnostic::error(
                DotagentsDiagnosticCode::ParseError,
                format!("model preset parameter `{key}` must be {expected}, found {value}"),
            )
            .with_source(self.source.clone())
        };

        // Phase 1: resolve every parameter into a typed value before touching
        // `params`. A failure anywhere leaves the caller's explicit base
        // configuration completely unchanged (no partial overlays).
        let mut temperature = None;
        let mut top_p = None;
        let mut top_k = None;
        let mut max_tokens = None;
        let mut timeout_seconds = None;
        let mut base_url = None;
        let mut custom: Vec<(String, serde_json::Value)> = Vec::new();

        for (key, value) in &self.parameters {
            match key.as_str() {
                "temperature" => {
                    temperature = Some(
                        value
                            .as_f64()
                            .map(|v| v as f32)
                            .ok_or_else(|| invalid(key, value, "a number"))?,
                    )
                }
                "top_p" => {
                    top_p = Some(
                        value
                            .as_f64()
                            .map(|v| v as f32)
                            .ok_or_else(|| invalid(key, value, "a number"))?,
                    )
                }
                "top_k" => {
                    top_k = Some(
                        value
                            .as_u64()
                            .and_then(|v| u32::try_from(v).ok())
                            .ok_or_else(|| invalid(key, value, "a non-negative integer"))?,
                    )
                }
                "max_tokens" => {
                    max_tokens = Some(
                        value
                            .as_u64()
                            .and_then(|v| u32::try_from(v).ok())
                            .ok_or_else(|| invalid(key, value, "a non-negative integer"))?,
                    )
                }
                "timeout_seconds" => {
                    timeout_seconds = Some(
                        value
                            .as_u64()
                            .ok_or_else(|| invalid(key, value, "a non-negative integer"))?,
                    )
                }
                "base_url" => {
                    let Some(text) = value.as_str() else {
                        return Err(invalid(key, value, "a string"));
                    };
                    base_url = Some(text.to_string());
                }
                _ => custom.push((key.clone(), value.clone())),
            }
        }

        // Phase 2: all parameters are valid, so commit the overlay.
        params.provider = Some(self.provider.clone());
        params.model = Some(self.model.clone());
        if let Some(api_key) = &self.api_key {
            params.api_key = Some(api_key.clone());
        }
        if let Some(value) = temperature {
            params.temperature = Some(value);
        }
        if let Some(value) = top_p {
            params.top_p = Some(value);
        }
        if let Some(value) = top_k {
            params.top_k = Some(value);
        }
        if let Some(value) = max_tokens {
            params.max_tokens = Some(value);
        }
        if let Some(value) = timeout_seconds {
            params.timeout_seconds = Some(value);
        }
        if let Some(value) = base_url {
            params.base_url = Some(value);
        }
        if !custom.is_empty() {
            let target = params.custom.get_or_insert_with(HashMap::new);
            for (key, value) in custom {
                target.insert(key, value);
            }
        }

        Ok(())
    }
}
