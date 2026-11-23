use std::time::Duration;

use clap::{Subcommand, ValueEnum};
use qmacp::{client::AcpClient, error::CliError, session::extension_payload};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const VERSION: u64 = 1;
const NODES: &str = "querymt/mesh/nodes";
const GET: &str = "querymt/mesh/getPeerAccess";
const SET: &str = "querymt/mesh/setPeerAccess";
const SET_QUOTA: &str = "querymt/mesh/setPeerQuota";
const RESET_QUOTA: &str = "querymt/mesh/resetPeerQuota";
const CHANGED: &str = "querymt/mesh/peerAccessChanged";

#[derive(Debug, Subcommand)]
pub(crate) enum MeshCommand {
    /// List mesh nodes and any saved peer IDs available to this connection.
    Nodes,
    /// Read both directions of sharing and quota status for a peer ID.
    Get { peer_id: String },
    /// Replace a peer's grant with the supplied JSON object.
    Set {
        peer_id: String,
        #[arg(long)]
        expected_revision: String,
        /// A No access, Full access, or Custom grant object.
        #[arg(long, value_name = "JSON")]
        grant: String,
    },
    /// Configure one provider or model quota.
    SetQuota {
        peer_id: String,
        #[arg(long)]
        expected_revision: String,
        #[command(flatten)]
        scope: ScopeArgs,
        /// Positive whole tokens, optionally scaled by k or M (e.g. 2500000, 500k, 2.5M).
        #[arg(
            long,
            conflicts_with = "unlimited",
            required_unless_present = "unlimited"
        )]
        tokens: Option<String>,
        #[arg(long, conflicts_with = "window")]
        unlimited: bool,
        #[arg(long, value_enum)]
        window: Option<Window>,
        /// Update an amount in place or explicitly start a new accounting period.
        #[arg(long, value_enum)]
        operation: Operation,
    },
    /// Reset one existing finite quota, without resetting other scopes.
    ResetQuota {
        peer_id: String,
        #[arg(long)]
        expected_revision: String,
        #[command(flatten)]
        scope: ScopeArgs,
    },
    /// Wait for one peer sharing invalidation, then exit. Use get to refresh.
    Watch {
        peer_id: String,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
    },
}

#[derive(Debug, clap::Args)]
pub(crate) struct ScopeArgs {
    #[arg(long)]
    provider: String,
    #[arg(long)]
    model: Option<String>,
}

impl ScopeArgs {
    fn params(self) -> Result<Value, CliError> {
        nonempty(&self.provider, "provider ID")?;
        let mut scope = json!({"provider_id": self.provider});
        if let Some(model) = self.model {
            nonempty(&model, "model ID")?;
            scope["model_id"] = json!(model);
        }
        Ok(scope)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum Window {
    Manual,
    Daily,
    Weekly,
    Monthly,
}

impl Window {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum Operation {
    Update,
    Reconfigure,
}

pub(crate) async fn run(client: &AcpClient, command: MeshCommand) -> Result<Value, CliError> {
    let (method, params) = match command {
        MeshCommand::Nodes => (NODES, json!({})),
        MeshCommand::Get { peer_id } => {
            nonempty(&peer_id, "peer ID")?;
            (GET, json!({"version": VERSION, "peer_id": peer_id}))
        }
        MeshCommand::Set {
            peer_id,
            expected_revision,
            grant,
        } => {
            let mut params = mutation_params(peer_id, expected_revision)?;
            params["grant"] = parse_grant(&grant)?;
            (SET, params)
        }
        MeshCommand::SetQuota {
            peer_id,
            expected_revision,
            scope,
            tokens,
            unlimited,
            window,
            operation,
        } => {
            let limit = if unlimited {
                if matches!(operation, Operation::Update) {
                    return Err(CliError::Usage(
                        "unlimited quota requires --operation reconfigure".into(),
                    ));
                }
                json!({"kind": "unlimited"})
            } else {
                let tokens = parse_tokens(tokens.as_deref().unwrap_or_default())?;
                let mut limit = json!({"kind": "finite", "limit_tokens": tokens.to_string()});
                if let Some(window) = window {
                    limit["window"] = json!(window.as_str());
                }
                limit
            };
            let mut params = mutation_params(peer_id, expected_revision)?;
            params["scope"] = scope.params()?;
            params["operation"] = json!(match operation {
                Operation::Update => "update",
                Operation::Reconfigure => "reconfigure",
            });
            params["limit"] = limit;
            (SET_QUOTA, params)
        }
        MeshCommand::ResetQuota {
            peer_id,
            expected_revision,
            scope,
        } => {
            let mut params = mutation_params(peer_id, expected_revision)?;
            params["scope"] = scope.params()?;
            (RESET_QUOTA, params)
        }
        MeshCommand::Watch { .. } => {
            return Err(CliError::Usage(
                "mesh watch must be run outside exec".into(),
            ));
        }
    };
    require_capability(client, "methods", method, method != NODES).await?;
    let response = client
        .extension(method, params.clone())
        .await
        .map_err(|err| {
            if params.get("expected_revision").is_some() {
                let server_error = err.downcast_ref::<qmacp::client::RpcResponseError>();
                let data = server_error.and_then(|err| err.data());
                let rejected = server_error.is_some_and(|err| err.is_rejection())
                    || data
                        .and_then(|data| data.get("code"))
                        .and_then(Value::as_str)
                        .is_some_and(|code| {
                            matches!(
                                code,
                                "mesh_access_conflict"
                                    | "mesh_quota_reconfigure_required"
                                    | "mesh_quota_in_flight"
                                    | "mesh_access_denied"
                            )
                        });
                let mut recovery = json!({
                    "peerId": params["peer_id"],
                    "expectedRevision": params["expected_revision"],
                    "deliveryState": if rejected { "rejected" } else { "unknown" },
                });
                if let Some(data) = data {
                    recovery["rpcData"] = data.clone();
                }
                CliError::from_request(err).with_recovery(recovery)
            } else {
                CliError::from_request(err)
            }
        })?;
    let result = extension_payload(&response);
    if !result.is_object() || (method != NODES && result["version"].as_u64() != Some(VERSION)) {
        let mut error = CliError::rpc("invalid mesh method response");
        if params.get("expected_revision").is_some() {
            error = error.with_recovery(json!({
                "peerId": params["peer_id"],
                "expectedRevision": params["expected_revision"],
                "deliveryState": "unknown",
            }));
        }
        return Err(error);
    }
    Ok(result)
}

pub(crate) async fn watch(
    client: &AcpClient,
    mut events: mpsc::Receiver<Value>,
    peer_id: &str,
    timeout: Duration,
) -> Result<Value, CliError> {
    nonempty(peer_id, "peer ID")?;
    tokio::time::timeout(timeout, async {
        let capability = require_capability(client, "notifications", CHANGED, true);
        tokio::pin!(capability);
        let mut hint = None;
        loop {
            tokio::select! {
                supported = &mut capability => { supported?; break; }
                event = events.recv() => record_hint(event, peer_id, &mut hint)?,
            }
        }
        while hint.is_none() {
            record_hint(events.recv().await, peer_id, &mut hint)?;
        }
        Ok(hint)
    })
    .await
    .map_err(|_| {
        CliError::Timeout(format!(
            "no mesh access change for {peer_id} within {}s",
            timeout.as_secs()
        ))
    })??
    .ok_or_else(|| CliError::rpc("missing matched mesh notification"))
}

fn record_hint(
    event: Option<Value>,
    peer_id: &str,
    hint: &mut Option<Value>,
) -> Result<(), CliError> {
    let event =
        event.ok_or_else(|| CliError::Connection("mesh notification stream closed".into()))?;
    if hint.is_none() && event["type"] == "mesh_peer_access_changed" && event["peer_id"] == peer_id
    {
        *hint = Some(event);
    }
    Ok(())
}

async fn require_capability(
    client: &AcpClient,
    list: &str,
    name: &str,
    needs_version: bool,
) -> Result<(), CliError> {
    client.initialize().await.map_err(CliError::from_request)?;
    let caps = client
        .extension("querymt/capabilities", json!({}))
        .await
        .map_err(CliError::from_request)?;
    let caps = extension_payload(&caps);
    let advertised = caps[list]
        .as_array()
        .is_some_and(|entries| entries.iter().any(|method| method == name));
    if advertised && (!needs_version || caps["mesh_access_version"].as_u64() == Some(VERSION)) {
        Ok(())
    } else {
        Err(CliError::rpc(format!(
            "server does not advertise support for {name} on this connection"
        )))
    }
}

fn mutation_params(peer_id: String, expected_revision: String) -> Result<Value, CliError> {
    nonempty(&peer_id, "peer ID")?;
    nonempty(&expected_revision, "revision")?;
    Ok(json!({"version": VERSION, "peer_id": peer_id, "expected_revision": expected_revision}))
}

fn nonempty(value: &str, label: &str) -> Result<(), CliError> {
    if value.trim().is_empty() {
        Err(CliError::Usage(format!("{label} must not be empty")))
    } else {
        Ok(())
    }
}

fn parse_grant(value: &str) -> Result<Value, CliError> {
    let grant: Value = serde_json::from_str(value)
        .map_err(|err| CliError::Usage(format!("invalid grant JSON: {err}")))?;
    match grant["mode"].as_str() {
        Some("full" | "none") => {}
        Some("custom") => {
            let shell = match grant.get("shell") {
                None => false,
                Some(Value::Bool(shell)) => *shell,
                _ => {
                    return Err(CliError::Usage(
                        "custom grant shell must be a boolean".into(),
                    ));
                }
            };
            let filesystem = grant["filesystem"].as_bool() == Some(true);
            if shell && !filesystem {
                return Err(CliError::Usage("shell requires filesystem access".into()));
            }
            if filesystem && grant["sessions"].as_bool() != Some(true) {
                return Err(CliError::Usage("filesystem requires session access".into()));
            }
        }
        _ => {
            return Err(CliError::Usage(
                "grant mode must be none, custom, or full".into(),
            ));
        }
    }
    Ok(grant)
}

fn parse_tokens(value: &str) -> Result<i64, CliError> {
    fn digits(value: &str) -> bool {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
    }

    let error =
        || CliError::Usage("--tokens must specify 1 to 9223372036854775807 whole tokens".into());
    let (value, places) = if let Some(value) = value.strip_suffix('k') {
        (value, 3)
    } else if let Some(value) = value.strip_suffix('M') {
        (value, 6)
    } else {
        (value, 0)
    };
    let (whole, fractional) = value
        .split_once('.')
        .map_or((value, None), |(whole, fraction)| (whole, Some(fraction)));
    if !digits(whole) {
        return Err(error());
    }
    let mut tokens = whole
        .parse::<i64>()
        .ok()
        .and_then(|n| n.checked_mul(10_i64.pow(places)))
        .ok_or_else(error)?;
    if let Some(fractional) = fractional {
        if places == 0 || !digits(fractional) {
            return Err(error());
        }
        // Extra trailing zeroes do not change the exact token count.
        let fractional = fractional.trim_end_matches('0');
        if !fractional.is_empty() {
            let len = u32::try_from(fractional.len()).map_err(|_| error())?;
            let padding = places.checked_sub(len).ok_or_else(error)?;
            let fraction = fractional.parse::<i64>().map_err(|_| error())?;
            tokens = tokens
                .checked_add(fraction * 10_i64.pow(padding))
                .ok_or_else(error)?;
        }
    }
    (tokens > 0).then_some(tokens).ok_or_else(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_parse_without_rounding() {
        for (input, expected) in [
            ("2500000", 2_500_000),
            ("500k", 500_000),
            ("2.5M", 2_500_000),
            ("0.001k", 1),
            ("0.000001000M", 1),
            ("9007199254740993", 9_007_199_254_740_993),
            ("9223372036854775807", i64::MAX),
            ("9223372036854775.807k", i64::MAX),
            ("9223372036854.775807M", i64::MAX),
        ] {
            assert_eq!(parse_tokens(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn token_counts_reject_invalid_fractional_and_overflowing_values() {
        for input in [
            "",
            "0",
            "0.000000M",
            "-1",
            "+1",
            " 1",
            "1_000",
            "1e6",
            "1m",
            "1K",
            "1.0",
            "2.5",
            "1.k",
            "1.2.3M",
            "0.0001k",
            "0.0000001M",
            "9223372036854775808",
            "9223372036854776k",
            "9223372036854775.808k",
            "9223372036854.775808M",
        ] {
            assert!(parse_tokens(input).is_err(), "{input}");
        }
    }
}
