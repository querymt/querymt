//! Building a runtime's root agent.
//!
//! [`build_root`] is the one way a root agent is made. It builds the members,
//! decides whether delegation is enabled, registers the delegation tool,
//! builds the handle and, when enabled, starts the delegation service on it.

use super::member::{DelegateBuildParams, MemberBuildSpec, MemberContext, build_local_member};
use super::roster::{AgentId, AgentRoster, Member, MemberAgent, MemberOrigin, RosterSnapshot};
use crate::agent::LocalAgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::builtin_tools::DelegateTool;
use crate::agent::core::ToolConfig;
use crate::config::{
    DelegationSettings, DelegationSummaryConfig, MiddlewareEntry, SkillsConfig, ToolRef,
};
use crate::delegation::{DelegationOrchestrator, DelegationSummarizer};
use crate::dotagents::{DotagentsSubAgentPlans, merge_protocol_targets};
use crate::middleware::MiddlewareDriver;
use crate::session::provider::SessionProvider;
use crate::session::store::SessionStore;
use anyhow::{Context, Result};
use querymt::plugin::host::PluginRegistry;
use std::path::PathBuf;
use std::sync::Arc;

/// Builds a middleware driver once the root's configuration exists.
pub type MiddlewareFactory = Box<dyn FnOnce(&LocalAgentHandle) -> Arc<dyn MiddlewareDriver> + Send>;

/// A delegate whose LLM calls run on a mesh peer.
#[cfg(feature = "remote")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRoute {
    pub delegate: AgentId,
    pub peer: String,
}

/// Everything `build_root` needs besides the root's own builder: the members
/// of the roster, the settings of the delegation service and the middleware
/// that is assembled once the root's configuration exists.
#[doc(hidden)]
pub struct RootParts {
    /// The builder's delegation flag. Delegation is enabled when this is on and
    /// the root's tool selection admits `delegate` (see [`delegation_enabled`]).
    pub enabled: bool,
    pub settings: DelegationSettings,
    /// Set for profile runtimes, so each service claims only its own sessions.
    pub profile_id: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Configured delegates, built before they enter the roster.
    pub members: Vec<MemberBuildSpec>,
    pub member_params: DelegateBuildParams,
    /// Members that already exist (remote agents), registered first.
    pub preregistered: Vec<Member>,
    /// Protocol delegation targets, added lazily when delegation is enabled.
    pub protocol_plans: Option<DotagentsSubAgentPlans>,
    /// The root's skills configuration; protocol targets inherit it.
    pub skills: Option<SkillsConfig>,
    pub middleware_factories: Vec<MiddlewareFactory>,
    pub middleware_entries: Vec<MiddlewareEntry>,
    #[cfg(feature = "remote")]
    pub peer_delegates: Vec<PeerRoute>,
    #[cfg(feature = "remote")]
    pub mesh: Option<crate::agent::remote::MeshHandle>,
}

/// The one rule for whether a runtime delegates: the flag is on and the root's
/// tool selection admits `delegate` (an empty selection admits everything).
pub fn delegation_enabled(flag: bool, tools: &ToolConfig) -> bool {
    flag && tools.allows(ToolRef::builtin(DelegateTool::NAME))
}

/// Builds the roster: pre-registered members first, then configured
/// delegates, which win, then protocol targets, which yield to both.
///
/// Configured members are built before they enter the roster, so a bad
/// middleware entry fails startup. Protocol targets are lowered now and
/// built on first use, on this runtime's event stream.
fn build_roster(
    ctx: MemberContext,
    parts: &mut RootParts,
    enabled: bool,
) -> Result<Arc<AgentRoster>> {
    let mut members = std::mem::take(&mut parts.preregistered);
    for spec in &parts.members {
        let handle =
            build_local_member(spec, &ctx).with_context(|| format!("delegate '{}'", spec.id))?;
        members.push(Member::built(
            spec.info(),
            MemberOrigin::Config,
            MemberAgent::Local(handle),
        ));
    }
    let mut snapshot = RosterSnapshot::from_members(members)?;

    if let (Some(plans), true) = (&parts.protocol_plans, enabled) {
        let merge = merge_protocol_targets(plans, Some(&snapshot));
        for diagnostic in &merge.diagnostics {
            log::warn!("dotagents: {diagnostic}");
        }
        for plan in merge.accepted {
            let info = plan.info.clone();
            let member = match MemberBuildSpec::from_protocol_plan(&plan, parts.skills.as_ref()) {
                Ok(spec) => Member::pending(info, MemberOrigin::Protocol, spec),
                Err(error) => {
                    Member::unbuildable(info, MemberOrigin::Protocol, &format!("{error:#}"))
                }
            };
            snapshot.insert(member)?;
        }
    }
    Ok(Arc::new(AgentRoster::new(ctx, snapshot, enabled)))
}

/// Builds the root agent of a runtime.
///
/// The builder carries the root's own configuration. On return the handle owns
/// a running delegation service when delegation is enabled; `shutdown` stops it.
#[doc(hidden)]
pub async fn build_root(
    mut builder: AgentConfigBuilder,
    mut parts: RootParts,
) -> Result<Arc<LocalAgentHandle>> {
    let enabled = delegation_enabled(parts.enabled, builder.tool_config());
    let storage = builder.storage().clone();
    let plugin_registry = parts.member_params.plugin_registry.clone();
    let member_ctx = MemberContext::new(
        parts.member_params.clone(),
        parts.settings.clone(),
        storage.clone(),
        builder.event_fanout(),
    );
    #[cfg(feature = "remote")]
    member_ctx.set_mesh(parts.mesh.clone());
    let roster = build_roster(member_ctx, &mut parts, enabled)?;
    let delegation = parts;

    builder = builder
        .with_roster(roster.clone())
        .with_delegation_wait_policy(delegation.settings.wait_policy.clone())
        .with_delegation_wait_timeout_secs(delegation.settings.wait_timeout_secs)
        .with_delegation_cancel_grace_secs(delegation.settings.cancel_grace_secs);
    if enabled {
        builder
            .tool_registry_mut()
            .add(Arc::new(DelegateTool::new(roster.clone())));
    }

    #[cfg(feature = "remote")]
    let routing = if enabled {
        setup_peer_routing(&delegation.peer_delegates, delegation.mesh.as_ref()).await
    } else {
        None
    };
    #[cfg(feature = "remote")]
    if let Some(routing) = &routing {
        use crate::agent::builtin_tools::{RouteDelegationToPeerTool, UseRemoteProviderTool};
        builder.extend_tool_registry([
            Arc::new(RouteDelegationToPeerTool::new(routing.actor.clone()))
                as Arc<dyn crate::tools::Tool>,
            Arc::new(UseRemoteProviderTool::new(routing.actor.clone()))
                as Arc<dyn crate::tools::Tool>,
        ]);
    }

    // Programmatic middleware factories see a temporary handle on the config
    // built so far; config entries and the defaults share one assembly path.
    let initial_config = Arc::new(builder.build());
    let temp_handle = Arc::new(LocalAgentHandle::from_config(initial_config.clone()));
    // Drivers the builder already holds come first; `with_middleware`
    // replaces the list, so they are carried over.
    let mut middleware_drivers: Vec<Arc<dyn MiddlewareDriver>> =
        initial_config.middleware_drivers.clone();
    for factory in delegation.middleware_factories {
        middleware_drivers.push(factory(&temp_handle));
    }
    let assembled = crate::middleware::build_from_entries(
        &delegation.middleware_entries,
        &middleware_drivers,
        &initial_config,
    )?;
    middleware_drivers.extend(assembled);
    // Drop the temporary handle so `Arc::try_unwrap` can succeed.
    drop(temp_handle);
    let initial = Arc::try_unwrap(initial_config).unwrap_or_else(|arc| (*arc).clone());
    let config = Arc::new(initial.with_middleware(middleware_drivers));
    let handle = Arc::new(LocalAgentHandle::from_config(config.clone()));

    if enabled {
        let settings = &delegation.settings;
        let summarizer = match &settings.summary {
            Some(summary) => {
                build_summarizer(summary, plugin_registry, storage.session_store()).await
            }
            None => None,
        };
        let mut orchestrator =
            DelegationOrchestrator::new(&handle, roster, settings, delegation.cwd.clone())
                .with_delegate_model_overrides(config.delegate_model_overrides.clone())
                .with_summarizer(summarizer)
                .with_budget_pricing(config.budget_pricing())
                .with_budget_cap(config.budget_cap_usd());
        if let Some(service) = config.workspace_service() {
            orchestrator = orchestrator.with_workspace_service(service);
        }
        #[cfg(feature = "remote")]
        if let Some(routing) = &routing {
            orchestrator = orchestrator.with_routing_snapshot(routing.snapshot.clone());
        }
        if let Some(profile_id) = delegation.profile_id {
            orchestrator = orchestrator.with_profile_id(profile_id);
        }
        let orchestrator = Arc::new(orchestrator);
        let listener = orchestrator.start_listening(config.event_sink.fanout());
        handle.install_delegation_service(orchestrator, listener);
    }

    #[cfg(feature = "remote")]
    if let (Some(routing), Some(mesh)) = (routing, delegation.mesh) {
        spawn_peer_event_resolver(routing.actor, mesh, delegation.peer_delegates);
    }

    Ok(handle)
}

/// Builds the summarizer that writes the brief handed to a delegate. A failure
/// is logged and delegation proceeds without summaries.
async fn build_summarizer(
    config: &DelegationSummaryConfig,
    plugin_registry: Arc<PluginRegistry>,
    store: Arc<dyn SessionStore>,
) -> Option<Arc<DelegationSummarizer>> {
    if !config.enabled {
        log::debug!("Delegation summarizer disabled in config");
        return None;
    }
    let provider = SessionProvider::new(plugin_registry, store, querymt::LLMParams::default());
    match DelegationSummarizer::from_config(config, &provider).await {
        Ok(summarizer) => {
            log::info!(
                "Delegation summarizer enabled with provider: {}, model: {}",
                config.provider,
                config.model
            );
            Some(Arc::new(summarizer))
        }
        Err(e) => {
            log::warn!("Failed to build delegation summarizer: {e}. Proceeding without summary.");
            None
        }
    }
}

/// The routing actor for delegates that run on a peer, and the snapshot the
/// delegation service reads at delegation time.
#[cfg(feature = "remote")]
struct PeerRouting {
    pub actor: kameo::actor::ActorRef<crate::agent::remote::routing::RoutingActor>,
    pub snapshot: crate::agent::remote::RoutingSnapshotHandle,
}

/// Creates the routing actor when any delegate has a `peer`, and fills its
/// routes. Peers already visible in the mesh are resolved at once; the rest are
/// resolved by [`spawn_peer_event_resolver`] when they appear.
#[cfg(feature = "remote")]
async fn setup_peer_routing(
    peer_delegates: &[PeerRoute],
    mesh: Option<&crate::agent::remote::MeshHandle>,
) -> Option<PeerRouting> {
    use crate::agent::remote::routing::{
        ResolvePeer, RouteTarget, RoutingActor, SetProviderTarget, new_routing_snapshot_handle,
    };
    use kameo::actor::Spawn;

    if peer_delegates.is_empty() {
        return None;
    }
    let snapshot = new_routing_snapshot_handle();
    let actor = RoutingActor::spawn(RoutingActor::new(snapshot.clone()));

    for PeerRoute {
        delegate: delegate_id,
        peer: peer_name,
    } in peer_delegates
    {
        if let Err(e) = actor
            .ask(SetProviderTarget {
                agent_id: delegate_id.to_string(),
                target: RouteTarget::Peer(peer_name.clone()),
            })
            .await
        {
            log::warn!("RoutingActor: failed to set provider route for '{delegate_id}': {e:?}");
        }

        let Some(mesh) = mesh else { continue };
        if let Some(node_id) = mesh.resolve_peer_node_id(peer_name).await {
            log::info!(
                "peer_delegate '{delegate_id}': resolved peer '{peer_name}' → node_id={node_id} (routing table)"
            );
            if let Err(e) = actor
                .ask(ResolvePeer {
                    peer_name: peer_name.clone(),
                    node_id: node_id.to_string(),
                })
                .await
            {
                log::warn!("RoutingActor: failed to resolve peer '{peer_name}': {e:?}");
            }
        } else {
            log::warn!(
                "peer_delegate '{delegate_id}': peer '{peer_name}' not found in mesh at build time. \
                 Will be resolved eagerly on PeerEvent::Discovered."
            );
        }
    }
    Some(PeerRouting { actor, snapshot })
}

/// Keeps the routing table current: resolves peers as they are discovered and
/// forgets them when they expire.
#[cfg(feature = "remote")]
fn spawn_peer_event_resolver(
    actor: kameo::actor::ActorRef<crate::agent::remote::routing::RoutingActor>,
    mesh: crate::agent::remote::MeshHandle,
    peer_delegates: Vec<PeerRoute>,
) {
    tokio::spawn(async move {
        use crate::agent::remote::routing::{ResolvePeer, UnresolvePeer};
        use crate::agent::remote::{GetNodeInfo, PeerEvent, RemoteNodeManager};
        let mut rx = mesh.subscribe_peer_events();
        loop {
            match rx.recv().await {
                Ok(PeerEvent::Discovered(peer_id)) => {
                    let mut node_manager = None;
                    for scope in mesh.active_scopes() {
                        let per_peer_name =
                            crate::agent::remote::scope::scoped_node_manager_for_peer(
                                &scope,
                                &peer_id.to_string(),
                            );
                        if let Ok(Some(nm)) =
                            mesh.lookup_actor::<RemoteNodeManager>(&per_peer_name).await
                        {
                            node_manager = Some(nm);
                            break;
                        }
                    }
                    let Some(node_manager) = node_manager else {
                        continue;
                    };
                    let node_info = match querymt_remote::ask_remote_with_timeout(
                        &node_manager,
                        &GetNodeInfo,
                        std::time::Duration::from_secs(3),
                    )
                    .await
                    {
                        Ok(info) => info,
                        Err(e) => {
                            log::debug!(
                                "RoutingActor PeerEvent handler: GetNodeInfo failed for {peer_id}: {e}"
                            );
                            continue;
                        }
                    };
                    for PeerRoute {
                        peer: peer_name, ..
                    } in &peer_delegates
                    {
                        if node_info.hostname == *peer_name
                            && let Err(e) = actor
                                .ask(ResolvePeer {
                                    peer_name: peer_name.clone(),
                                    node_id: node_info.node_id.to_string(),
                                })
                                .await
                        {
                            log::warn!("RoutingActor PeerEvent handler: ResolvePeer failed: {e:?}");
                        }
                    }
                }
                Ok(PeerEvent::Expired(peer_id)) => {
                    let node_id = peer_id.to_string();
                    if let Err(e) = actor
                        .ask(UnresolvePeer {
                            node_id: node_id.clone(),
                        })
                        .await
                    {
                        log::warn!(
                            "RoutingActor PeerEvent handler: UnresolvePeer failed for {node_id}: {e:?}"
                        );
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("RoutingActor PeerEvent handler: lagged, skipped {n} events");
                }
            }
        }
    });
}
