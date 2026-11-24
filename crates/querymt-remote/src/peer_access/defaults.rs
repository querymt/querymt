//! Host defaults for peers without a saved grant.
//!
//! A saved (explicit) grant always wins. Without one, a peer receives the union
//! of the defaults that currently apply to it:
//!
//! - `lan`: the peer has a live connection from a private-network address
//!   (RFC 1918, loopback, link-local, or IPv6 unique-local). Off by default.
//! - `trust:<scope>`: during this connection, the peer presented a valid token
//!   for a configured trust scope. An offline authority signs the token for
//!   one peer ID. It works over any transport and needs no internet access.
//!
//! Defaults are not stored. They track live connections, so they end when the
//! peer disconnects or its token expires.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};

use super::{GrantSource, PeerGrant, PeerGrantKey, valid_id};
use crate::NodeId;
use crate::invite::{Engine, InviteError, MembershipToken, URL_SAFE_NO_PAD};

/// The source label of the LAN default.
pub const LAN_DEFAULT: &str = "lan";
/// The prefix of encoded trust tokens.
pub const TRUST_TOKEN_PREFIX: &str = "qmt-trust:";
/// The `invite_id` that marks a membership token as a trust token.
const TRUST_TOKEN_MARKER: &str = "trust";
/// The number of tokens a peer may present in one exchange.
pub const MAX_PRESENTED_TOKENS: usize = 16;
/// The encoded size limit for one presented token.
pub const MAX_TOKEN_LEN: usize = 4096;
/// The longest a membership expiry timer sleeps before rechecking the wall
/// clock, so clock steps and system suspend delay an expiry by at most this.
#[cfg(feature = "kameo-mesh")]
const EXPIRY_RECHECK_SECS: u64 = 60;

/// Host defaults for peers that have no saved grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeshAccessConfig {
    /// Access for peers with a live private-network LAN connection. Default: No access.
    pub lan: PeerGrant,
    /// Trust scopes: what this host gives each scope's members, and the tokens
    /// this node presents as a member.
    pub trust: Vec<TrustScopeConfig>,
}

/// One trust scope, identified by name and authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustScopeConfig {
    /// Name of the scope. Tokens are bound to it.
    pub name: String,
    /// The peer ID of the key that signs membership tokens for this scope.
    pub authority: String,
    /// Access this host gives members. `None` makes this a membership-only entry.
    pub access: PeerGrant,
    /// This node's token for the scope, presented to the peers it connects to.
    pub token: Option<String>,
}

impl MeshAccessConfig {
    /// Validate grants, scope names, authorities, and the shape of local tokens.
    pub fn validate(&self) -> Result<(), String> {
        self.lan
            .validate()
            .map_err(|error| format!("invalid LAN default access: {error}"))?;
        let own_tokens = self.trust.iter().filter(|s| s.token.is_some()).count();
        if own_tokens > MAX_PRESENTED_TOKENS {
            return Err(format!(
                "at most {MAX_PRESENTED_TOKENS} trust scopes can have a token; {own_tokens} do"
            ));
        }
        let mut names = std::collections::HashSet::new();
        for scope in &self.trust {
            if !valid_id(&scope.name) {
                return Err("trust scope names must be non-empty without surrounding whitespace or control characters".into());
            }
            if !names.insert(scope.name.as_str()) {
                return Err(format!("duplicate trust scope '{}'", scope.name));
            }
            NodeId::parse(&scope.authority).map_err(|error| {
                format!(
                    "trust scope '{}' has an invalid authority: {error}",
                    scope.name
                )
            })?;
            scope
                .access
                .validate()
                .map_err(|error| format!("trust scope '{}': {error}", scope.name))?;
            if let Some(token) = &scope.token {
                let token = decode_trust_token(token)
                    .map_err(|error| format!("trust scope '{}': {error}", scope.name))?;
                if token.mesh_id != trust_scope_id(&scope.authority, &scope.name)
                    || token.admitted_by != scope.authority
                {
                    return Err(format!(
                        "trust scope '{}': the token was issued for another scope or authority",
                        scope.name
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether any default can grant access.
    pub fn grants_anything(&self) -> bool {
        self.lan != PeerGrant::None || self.trust.iter().any(|s| s.access != PeerGrant::None)
    }
}

/// The signed identity of a trust scope. It is distinct from Iroh mesh IDs.
pub fn trust_scope_id(authority: &str, name: &str) -> String {
    format!("trust:{authority}:{name}")
}

/// Decode a token without verifying it.
pub fn decode_trust_token(token: &str) -> Result<MembershipToken, InviteError> {
    let token = token.trim();
    let encoded = token.strip_prefix(TRUST_TOKEN_PREFIX).ok_or_else(|| {
        InviteError::InvalidToken(format!("trust tokens start with '{TRUST_TOKEN_PREFIX}'"))
    })?;
    if token.len() > MAX_TOKEN_LEN {
        return Err(InviteError::InvalidToken("trust token is too long".into()));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| InviteError::InvalidToken(format!("invalid trust token: {error}")))?;
    let token: MembershipToken = serde_json::from_slice(&bytes)
        .map_err(|error| InviteError::InvalidToken(format!("invalid trust token: {error}")))?;
    if token.invite_id != TRUST_TOKEN_MARKER || !token.mesh_id.starts_with("trust:") {
        return Err(InviteError::InvalidToken(
            "not a trust scope token".to_string(),
        ));
    }
    Ok(token)
}

#[cfg(feature = "kameo-mesh")]
fn encode_trust_token(token: &MembershipToken) -> Result<String, InviteError> {
    let json = serde_json::to_vec(token)
        .map_err(|error| InviteError::InvalidToken(format!("cannot encode token: {error}")))?;
    Ok(format!(
        "{TRUST_TOKEN_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(json)
    ))
}

/// Sign a token for `peer` as a member of the authority's `scope`.
/// `ttl_secs = None` issues a token without expiry.
#[cfg(feature = "kameo-mesh")]
pub fn issue_trust_token(
    authority: &libp2p::identity::Keypair,
    scope: &str,
    peer: &str,
    ttl_secs: Option<u64>,
) -> Result<String, InviteError> {
    if !valid_id(scope) {
        return Err(InviteError::InvalidToken("invalid trust scope name".into()));
    }
    NodeId::parse(peer).map_err(InviteError::InvalidToken)?;
    let authority_id = authority.public().to_peer_id().to_string();
    let expires_at = ttl_secs.map_or(0, |ttl| now_secs().saturating_add(ttl));
    let token = MembershipToken::issue(
        trust_scope_id(&authority_id, scope),
        peer,
        authority,
        TRUST_TOKEN_MARKER.to_string(),
        crate::invite::InvitePermissions::default(),
        expires_at,
    )?;
    let encoded = encode_trust_token(&token)?;
    // Hosts reject longer tokens, so never issue one.
    if encoded.len() > MAX_TOKEN_LEN {
        return Err(InviteError::InvalidToken(
            "trust token is too long; use a shorter scope name".into(),
        ));
    }
    Ok(encoded)
}

struct TrustScope {
    name: String,
    #[cfg_attr(not(feature = "kameo-mesh"), allow(dead_code))]
    authority: String,
    access: PeerGrant,
}

#[derive(Default)]
struct State {
    host: Option<NodeId>,
    /// Advanced by every `configure`. Verification done under an older
    /// configuration is discarded.
    generation: u64,
    lan: PeerGrant,
    scopes: Vec<TrustScope>,
    own_tokens: Vec<String>,
    lan_connections: HashMap<NodeId, usize>,
    /// A session ID for each connected peer, from its first connection until
    /// its last one closes. Verification for an older session is discarded.
    sessions: HashMap<NodeId, u64>,
    next_session: u64,
    /// Scope name to expiry (0 = never) for each peer, for the current connection.
    members: HashMap<NodeId, BTreeMap<String, u64>>,
    /// At most one expiry timer per peer, cancelled when its memberships end.
    expiry_timers: HashMap<NodeId, tokio::task::AbortHandle>,
}

impl State {
    fn cancel_expiry_timer(&mut self, peer: &NodeId) {
        if let Some(timer) = self.expiry_timers.remove(peer) {
            timer.abort();
        }
    }

    /// The earliest finite expiry of the peer's memberships that has not passed.
    #[cfg(feature = "kameo-mesh")]
    fn next_expiry(&self, peer: &NodeId, now: u64) -> Option<u64> {
        self.members
            .get(peer)?
            .values()
            .copied()
            .filter(|&expires| expires != 0 && now <= expires)
            .min()
    }
}

/// Tokens verified for one connection session under one configuration.
#[cfg(feature = "kameo-mesh")]
struct Verified {
    session: u64,
    generation: u64,
    accepted: BTreeMap<String, u64>,
}

type Publisher = Arc<dyn Fn(&NodeId) + Send + Sync>;

#[derive(Default)]
struct Shared {
    state: RwLock<State>,
    publisher: Mutex<Option<Publisher>>,
}

/// Live default access for one host identity. Cloning shares the same state.
#[derive(Clone, Default)]
pub struct AccessDefaults(Arc<Shared>);

/// A non-owning handle to [`AccessDefaults`].
#[derive(Clone)]
pub(crate) struct WeakAccessDefaults(Weak<Shared>);

impl WeakAccessDefaults {
    pub(crate) fn upgrade(&self) -> Option<AccessDefaults> {
        self.0.upgrade().map(AccessDefaults)
    }
}

impl std::fmt::Debug for AccessDefaults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.0.state.read();
        f.debug_struct("AccessDefaults")
            .field("host", &state.host)
            .field("lan", &state.lan)
            .field("scopes", &state.scopes.len())
            .finish_non_exhaustive()
    }
}

impl AccessDefaults {
    pub(crate) fn downgrade(&self) -> WeakAccessDefaults {
        WeakAccessDefaults(Arc::downgrade(&self.0))
    }

    /// Install the hook that publishes a peer's new effective grant.
    pub(crate) fn set_publisher(&self, publisher: Publisher) {
        *self.0.publisher.lock() = Some(publisher);
    }

    /// Apply validated defaults for `host`, replacing any previous ones.
    pub fn configure(&self, host: NodeId, config: &MeshAccessConfig) -> Result<(), String> {
        config.validate()?;
        let peers: HashSet<NodeId> = {
            let mut state = self.0.state.write();
            let mut peers: HashSet<NodeId> = state.lan_connections.keys().cloned().collect();
            if state.host.as_ref().is_some_and(|current| *current != host) {
                // Connections tracked for another identity are not this host's.
                state.lan_connections.clear();
                state.sessions.clear();
            }
            state.host = Some(host);
            state.generation = state.generation.wrapping_add(1);
            state.lan = config.lan.clone();
            state.scopes = config
                .trust
                .iter()
                .map(|scope| TrustScope {
                    name: scope.name.clone(),
                    authority: scope.authority.clone(),
                    access: scope.access.clone(),
                })
                .collect();
            state.own_tokens = config
                .trust
                .iter()
                .filter_map(|scope| scope.token.as_deref())
                .map(|token| token.trim().to_string())
                .collect();
            // Memberships verified under the old scopes no longer apply.
            peers.extend(state.members.drain().map(|(peer, _)| peer));
            for (_, timer) in state.expiry_timers.drain() {
                timer.abort();
            }
            peers
        };
        for peer in peers {
            self.publish(&peer);
        }
        Ok(())
    }

    /// The host these defaults apply to, once configured.
    pub fn host(&self) -> Option<NodeId> {
        self.0.state.read().host.clone()
    }

    /// The tokens this node presents to every connected peer.
    pub fn own_tokens(&self) -> Vec<String> {
        self.0.state.read().own_tokens.clone()
    }

    /// Resolve the defaults that apply to `key` now.
    pub fn resolve(&self, key: &PeerGrantKey) -> (PeerGrant, GrantSource) {
        self.resolve_at(key, now_secs())
    }

    fn resolve_at(&self, key: &PeerGrantKey, now: u64) -> (PeerGrant, GrantSource) {
        let state = self.0.state.read();
        let mut grant = PeerGrant::None;
        let mut defaults = Vec::new();
        if state.host.as_ref() != Some(&key.host) {
            return (grant, GrantSource::Default { defaults });
        }
        if state.lan != PeerGrant::None && state.lan_connections.contains_key(&key.peer) {
            grant = grant.union(state.lan.clone());
            defaults.push(LAN_DEFAULT.to_string());
        }
        if let Some(memberships) = state.members.get(&key.peer) {
            for scope in &state.scopes {
                let active = memberships
                    .get(&scope.name)
                    .is_some_and(|&expires| expires == 0 || now <= expires);
                if active && scope.access != PeerGrant::None {
                    grant = grant.union(scope.access.clone());
                    defaults.push(format!("trust:{}", scope.name));
                }
            }
        }
        (grant, GrantSource::Default { defaults })
    }

    /// Record a new private-network LAN connection for `peer`.
    pub fn lan_connection_opened(&self, peer: &NodeId) {
        let changed = {
            let mut state = self.0.state.write();
            let count = state.lan_connections.entry(peer.clone()).or_default();
            *count += 1;
            *count == 1 && state.lan != PeerGrant::None
        };
        if changed {
            self.publish(peer);
        }
    }

    /// Record that a private-network LAN connection for `peer` closed.
    pub fn lan_connection_closed(&self, peer: &NodeId) {
        let changed = {
            let mut state = self.0.state.write();
            let Some(count) = state.lan_connections.get_mut(peer) else {
                return;
            };
            *count = count.saturating_sub(1);
            let gone = *count == 0;
            if gone {
                state.lan_connections.remove(peer);
            }
            gone && state.lan != PeerGrant::None
        };
        if changed {
            self.publish(peer);
        }
    }

    /// Record the peer's first live connection. Trust tokens are accepted only
    /// from connected peers, and only until their last connection closes.
    pub fn peer_connected(&self, peer: &NodeId) {
        let mut state = self.0.state.write();
        state.next_session = state.next_session.wrapping_add(1);
        let session = state.next_session;
        state.sessions.insert(peer.clone(), session);
    }

    /// Forget everything tied to the peer's connections.
    pub fn peer_disconnected(&self, peer: &NodeId) {
        let changed = {
            let mut state = self.0.state.write();
            state.sessions.remove(peer);
            state.cancel_expiry_timer(peer);
            let lan = state.lan_connections.remove(peer).is_some() && state.lan != PeerGrant::None;
            state.members.remove(peer).is_some() || lan
        };
        if changed {
            self.publish(peer);
        }
    }

    /// Verify tokens presented by the transport-authenticated `peer` and admit
    /// it to each matching scope for the current connection.
    /// Returns the names of the scopes the tokens prove membership of.
    #[cfg(feature = "kameo-mesh")]
    pub fn present(&self, peer: &NodeId, tokens: &[String]) -> Vec<String> {
        match self.verify(peer, tokens) {
            Some(verified) => self.commit(peer, verified),
            None => Vec::new(),
        }
    }

    /// Check the tokens against the current scopes. `None` if the peer has no
    /// live connection.
    #[cfg(feature = "kameo-mesh")]
    fn verify(&self, peer: &NodeId, tokens: &[String]) -> Option<Verified> {
        let state = self.0.state.read();
        let Some(&session) = state.sessions.get(peer) else {
            tracing::debug!(%peer, "ignoring trust tokens from a peer without a live connection");
            return None;
        };
        let mut accepted = BTreeMap::new();
        for encoded in tokens.iter().take(MAX_PRESENTED_TOKENS) {
            // Decoding trims the token and enforces MAX_TOKEN_LEN, as configuration does.
            let token = match decode_trust_token(encoded) {
                Ok(token) => token,
                Err(error) => {
                    tracing::debug!(%peer, %error, "ignoring malformed trust token");
                    continue;
                }
            };
            let Some(scope) = state.scopes.iter().find(|scope| {
                token.admitted_by == scope.authority
                    && token.mesh_id == trust_scope_id(&scope.authority, &scope.name)
            }) else {
                continue;
            };
            // The subject must be the authenticated sender, not a claimed ID.
            if token.peer_id != peer.to_string() {
                tracing::warn!(%peer, subject = %token.peer_id, scope = %scope.name, "rejecting a trust token issued to another peer");
                continue;
            }
            match token.verify() {
                Ok(()) => {
                    accepted.insert(scope.name.clone(), token.expires_at);
                }
                Err(error) => {
                    tracing::warn!(%peer, scope = %scope.name, %error, "rejecting trust token");
                }
            }
        }
        Some(Verified {
            session,
            generation: state.generation,
            accepted,
        })
    }

    /// Record verified memberships, unless the connection session or the
    /// configuration they were verified under has since ended.
    #[cfg(feature = "kameo-mesh")]
    fn commit(&self, peer: &NodeId, verified: Verified) -> Vec<String> {
        let Verified {
            session,
            generation,
            accepted,
        } = verified;
        let names: Vec<String> = accepted.keys().cloned().collect();
        let changed = {
            let mut state = self.0.state.write();
            if state.sessions.get(peer) != Some(&session) || state.generation != generation {
                tracing::debug!(%peer, "discarding trust verification for a closed connection or an old configuration");
                return Vec::new();
            }
            let entry = state.members.entry(peer.clone()).or_default();
            let mut changed = false;
            for (name, expires) in accepted {
                changed |= entry.insert(name, expires) != Some(expires);
            }
            if entry.is_empty() {
                state.members.remove(peer);
            }
            if changed {
                self.restart_expiry_timer(&mut state, peer);
            }
            changed
        };
        if changed {
            tracing::info!(%peer, scopes = ?names, "peer admitted to trust scopes");
            self.publish(peer);
        }
        names
    }

    #[cfg(feature = "kameo-mesh")]
    fn restart_expiry_timer(&self, state: &mut State, peer: &NodeId) {
        state.cancel_expiry_timer(peer);
        if state.next_expiry(peer, now_secs()).is_none() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let defaults = self.downgrade();
        let peer_id = peer.clone();
        let timer = runtime.spawn(async move {
            loop {
                let next = {
                    let Some(defaults) = defaults.upgrade() else {
                        return;
                    };
                    let Some(next) = defaults.0.state.read().next_expiry(&peer_id, now_secs())
                    else {
                        return;
                    };
                    next
                };
                // Expiry is wall-clock time, but sleeps are monotonic. Recheck
                // periodically so clock steps and suspend cannot extend access.
                let delay = next
                    .saturating_sub(now_secs())
                    .saturating_add(1)
                    .min(EXPIRY_RECHECK_SECS);
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                if now_secs() > next
                    && let Some(defaults) = defaults.upgrade()
                {
                    defaults.publish(&peer_id);
                }
            }
        });
        state
            .expiry_timers
            .insert(peer.clone(), timer.abort_handle());
    }

    /// Publish the peer's current effective grant to policy watchers. The
    /// publisher reads current state, so a delayed call is never stale.
    fn publish(&self, peer: &NodeId) {
        let Some(publisher) = self.0.publisher.lock().clone() else {
            return;
        };
        let peer = peer.clone();
        // The publisher reads the shared mesh state, which a slow save can hold.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(move || publisher(&peer));
            }
            Err(_) => publisher(&peer),
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(all(test, feature = "kameo-mesh"))]
mod tests {
    use super::*;
    use crate::peer_access::{ModelSelection, ProviderGrant};

    fn key(host: &libp2p::identity::Keypair, peer: &libp2p::identity::Keypair) -> PeerGrantKey {
        PeerGrantKey {
            host: NodeId::from_peer_id(host.public().to_peer_id()),
            peer: NodeId::from_peer_id(peer.public().to_peer_id()),
        }
    }

    fn providers_only(provider: &str, models: ModelSelection) -> PeerGrant {
        PeerGrant::Custom {
            sessions: false,
            filesystem: false,
            shell: false,
            providers: vec![ProviderGrant {
                provider_id: provider.into(),
                models,
            }],
        }
    }

    #[test]
    fn grant_union_allows_what_either_grant_allows() {
        let selected = |ids: &[&str]| ModelSelection::Selected {
            model_ids: ids.iter().map(|id| id.to_string()).collect(),
        };
        let a = providers_only("p", selected(&["m1"]));
        let b = PeerGrant::Custom {
            sessions: true,
            filesystem: true,
            shell: false,
            providers: vec![
                ProviderGrant {
                    provider_id: "p".into(),
                    models: selected(&["m1", "m2"]),
                },
                ProviderGrant {
                    provider_id: "q".into(),
                    models: ModelSelection::All,
                },
            ],
        };
        let union = a.clone().union(b);
        union.validate().unwrap();
        assert_eq!(
            union,
            PeerGrant::Custom {
                sessions: true,
                filesystem: true,
                shell: false,
                providers: vec![
                    ProviderGrant {
                        provider_id: "p".into(),
                        models: selected(&["m1", "m2"]),
                    },
                    ProviderGrant {
                        provider_id: "q".into(),
                        models: ModelSelection::All,
                    },
                ],
            }
        );
        assert_eq!(a.clone().union(PeerGrant::None), a);
        assert_eq!(PeerGrant::None.union(a.clone()), a);
        assert_eq!(a.clone().union(PeerGrant::Full), PeerGrant::Full);
        assert_eq!(
            a.union(providers_only("p", ModelSelection::All)),
            providers_only("p", ModelSelection::All)
        );
    }

    #[test]
    fn lan_default_applies_only_while_a_private_connection_is_open() {
        let host = libp2p::identity::Keypair::generate_ed25519();
        let peer = libp2p::identity::Keypair::generate_ed25519();
        let key = key(&host, &peer);
        let defaults = AccessDefaults::default();
        defaults
            .configure(
                key.host.clone(),
                &MeshAccessConfig {
                    lan: PeerGrant::Full,
                    trust: vec![],
                },
            )
            .unwrap();
        assert_eq!(defaults.resolve(&key).0, PeerGrant::None);
        defaults.lan_connection_opened(&key.peer);
        defaults.lan_connection_opened(&key.peer);
        assert_eq!(
            defaults.resolve(&key),
            (
                PeerGrant::Full,
                GrantSource::Default {
                    defaults: vec!["lan".into()]
                }
            )
        );
        defaults.lan_connection_closed(&key.peer);
        assert_eq!(defaults.resolve(&key).0, PeerGrant::Full);
        defaults.lan_connection_closed(&key.peer);
        assert_eq!(defaults.resolve(&key).0, PeerGrant::None);
        // Defaults belong to one host identity.
        defaults.lan_connection_opened(&key.peer);
        let other_host = PeerGrantKey {
            host: NodeId::from_peer_id(libp2p::PeerId::random()),
            peer: key.peer.clone(),
        };
        assert_eq!(defaults.resolve(&other_host).0, PeerGrant::None);
    }

    #[test]
    fn lan_default_is_off_unless_configured() {
        let host = libp2p::identity::Keypair::generate_ed25519();
        let peer = libp2p::identity::Keypair::generate_ed25519();
        let key = key(&host, &peer);
        let defaults = AccessDefaults::default();
        defaults
            .configure(key.host.clone(), &MeshAccessConfig::default())
            .unwrap();
        defaults.lan_connection_opened(&key.peer);
        assert_eq!(
            defaults.resolve(&key),
            (PeerGrant::None, GrantSource::Default { defaults: vec![] })
        );
    }

    #[test]
    fn trust_tokens_admit_only_the_authenticated_subject() {
        let authority = libp2p::identity::Keypair::generate_ed25519();
        let host = libp2p::identity::Keypair::generate_ed25519();
        let peer = libp2p::identity::Keypair::generate_ed25519();
        let other = libp2p::identity::Keypair::generate_ed25519();
        let key = key(&host, &peer);
        let authority_id = authority.public().to_peer_id().to_string();
        let access = providers_only("p", ModelSelection::All);
        let config = MeshAccessConfig {
            lan: PeerGrant::None,
            trust: vec![TrustScopeConfig {
                name: "eng".into(),
                authority: authority_id.clone(),
                access: access.clone(),
                token: None,
            }],
        };
        let defaults = AccessDefaults::default();
        defaults.configure(key.host.clone(), &config).unwrap();

        let token = issue_trust_token(&authority, "eng", &key.peer.to_string(), None).unwrap();
        // Tokens count only on a live connection.
        assert!(
            defaults
                .present(&key.peer, std::slice::from_ref(&token))
                .is_empty()
        );
        defaults.peer_connected(&key.peer);
        // A token issued to the peer does not admit another sender.
        let other_id = NodeId::from_peer_id(other.public().to_peer_id());
        defaults.peer_connected(&other_id);
        assert!(
            defaults
                .present(&other_id, std::slice::from_ref(&token))
                .is_empty()
        );
        // A token from another authority, or for another scope, admits nothing.
        let rogue = libp2p::identity::Keypair::generate_ed25519();
        let forged = issue_trust_token(&rogue, "eng", &key.peer.to_string(), None).unwrap();
        let wrong_scope =
            issue_trust_token(&authority, "ops", &key.peer.to_string(), None).unwrap();
        assert!(
            defaults
                .present(&key.peer, &[forged, wrong_scope])
                .is_empty()
        );
        assert_eq!(defaults.resolve(&key).0, PeerGrant::None);

        assert_eq!(
            defaults.present(&key.peer, std::slice::from_ref(&token)),
            vec!["eng"]
        );
        assert_eq!(
            defaults.resolve(&key),
            (
                access,
                GrantSource::Default {
                    defaults: vec!["trust:eng".into()]
                }
            )
        );
        defaults.peer_disconnected(&key.peer);
        assert_eq!(defaults.resolve(&key).0, PeerGrant::None);
        assert!(defaults.present(&key.peer, &[token]).is_empty());
    }

    #[test]
    fn tampered_and_expired_trust_tokens_are_rejected() {
        let authority = libp2p::identity::Keypair::generate_ed25519();
        let host = libp2p::identity::Keypair::generate_ed25519();
        let peer = libp2p::identity::Keypair::generate_ed25519();
        let key = key(&host, &peer);
        let defaults = AccessDefaults::default();
        defaults
            .configure(
                key.host.clone(),
                &MeshAccessConfig {
                    lan: PeerGrant::None,
                    trust: vec![TrustScopeConfig {
                        name: "eng".into(),
                        authority: authority.public().to_peer_id().to_string(),
                        access: PeerGrant::Full,
                        token: None,
                    }],
                },
            )
            .unwrap();
        defaults.peer_connected(&key.peer);
        let token = issue_trust_token(&authority, "eng", &key.peer.to_string(), None).unwrap();
        let mut decoded = decode_trust_token(&token).unwrap();
        // An unexpired, unsigned change: only the signature check can reject it.
        decoded.expires_at = now_secs() + 3600;
        let tampered = encode_trust_token(&decoded).unwrap();
        assert!(defaults.present(&key.peer, &[tampered]).is_empty());

        let mut expired = MembershipToken::issue(
            trust_scope_id(&authority.public().to_peer_id().to_string(), "eng"),
            &key.peer.to_string(),
            &authority,
            TRUST_TOKEN_MARKER.into(),
            Default::default(),
            1,
        )
        .unwrap();
        assert!(
            defaults
                .present(&key.peer, &[encode_trust_token(&expired).unwrap()])
                .is_empty()
        );
        // An Iroh membership token cannot be replayed as a trust token.
        expired.invite_id = "invite".into();
        assert!(decode_trust_token(&encode_trust_token(&expired).unwrap()).is_err());
        assert_eq!(defaults.resolve(&key).0, PeerGrant::None);
    }

    #[test]
    fn defaults_combine_and_validate() {
        let authority = libp2p::identity::Keypair::generate_ed25519();
        let host = libp2p::identity::Keypair::generate_ed25519();
        let peer = libp2p::identity::Keypair::generate_ed25519();
        let key = key(&host, &peer);
        let authority_id = authority.public().to_peer_id().to_string();
        let lan = providers_only("p", ModelSelection::All);
        let config = MeshAccessConfig {
            lan: lan.clone(),
            trust: vec![TrustScopeConfig {
                name: "eng".into(),
                authority: authority_id.clone(),
                access: PeerGrant::Custom {
                    sessions: true,
                    filesystem: false,
                    shell: false,
                    providers: vec![],
                },
                token: Some(
                    issue_trust_token(&authority, "eng", &key.host.to_string(), Some(60)).unwrap(),
                ),
            }],
        };
        config.validate().unwrap();
        let defaults = AccessDefaults::default();
        defaults.configure(key.host.clone(), &config).unwrap();
        assert_eq!(defaults.own_tokens().len(), 1);
        defaults.peer_connected(&key.peer);
        defaults.lan_connection_opened(&key.peer);
        let token = issue_trust_token(&authority, "eng", &key.peer.to_string(), Some(60)).unwrap();
        defaults.present(&key.peer, &[token]);
        let (grant, source) = defaults.resolve(&key);
        assert_eq!(
            source,
            GrantSource::Default {
                defaults: vec!["lan".into(), "trust:eng".into()]
            }
        );
        assert!(grant.allows_sessions(crate::peer_access::SharingProtocol::V1));
        assert!(grant.allows_model(crate::peer_access::SharingProtocol::V1, "p", "any"));

        let mut bad = config.clone();
        bad.trust[0].token =
            Some(issue_trust_token(&authority, "ops", &key.host.to_string(), None).unwrap());
        assert!(bad.validate().unwrap_err().contains("another scope"));
        let mut bad = config.clone();
        bad.trust.push(bad.trust[0].clone());
        assert!(bad.validate().unwrap_err().contains("duplicate"));
        let mut bad = config;
        bad.trust[0].authority = "nope".into();
        assert!(bad.validate().unwrap_err().contains("invalid authority"));
    }

    #[test]
    fn token_size_and_count_limits_match_what_hosts_accept() {
        let authority = libp2p::identity::Keypair::generate_ed25519();
        let authority_id = authority.public().to_peer_id().to_string();
        let host = NodeId::from_peer_id(libp2p::PeerId::random());
        // A token too long for hosts to decode is never issued.
        let long_scope = "s".repeat(MAX_TOKEN_LEN);
        assert!(issue_trust_token(&authority, &long_scope, &host.to_string(), None).is_err());

        // Padding is trimmed before the length check and before presenting.
        let token = issue_trust_token(&authority, "eng", &host.to_string(), None).unwrap();
        let padded = format!("{token}{}", " ".repeat(MAX_TOKEN_LEN));
        let scope = |name: &str, token: Option<String>| TrustScopeConfig {
            name: name.into(),
            authority: authority_id.clone(),
            access: PeerGrant::None,
            token,
        };
        let config = MeshAccessConfig {
            lan: PeerGrant::None,
            trust: vec![scope("eng", Some(padded))],
        };
        let defaults = AccessDefaults::default();
        defaults.configure(host.clone(), &config).unwrap();
        assert_eq!(defaults.own_tokens(), [token]);

        // A host reads at most MAX_PRESENTED_TOKENS tokens per exchange.
        let too_many = MeshAccessConfig {
            lan: PeerGrant::None,
            trust: (0..=MAX_PRESENTED_TOKENS)
                .map(|i| {
                    let name = format!("s{i}");
                    let token =
                        issue_trust_token(&authority, &name, &host.to_string(), None).unwrap();
                    scope(&name, Some(token))
                })
                .collect(),
        };
        assert!(too_many.validate().unwrap_err().contains("at most"));
    }

    fn trust_fixture() -> (
        libp2p::identity::Keypair,
        PeerGrantKey,
        MeshAccessConfig,
        AccessDefaults,
    ) {
        let authority = libp2p::identity::Keypair::generate_ed25519();
        let host = libp2p::identity::Keypair::generate_ed25519();
        let peer = libp2p::identity::Keypair::generate_ed25519();
        let key = key(&host, &peer);
        let config = MeshAccessConfig {
            lan: PeerGrant::None,
            trust: vec![TrustScopeConfig {
                name: "eng".into(),
                authority: authority.public().to_peer_id().to_string(),
                access: PeerGrant::Full,
                token: None,
            }],
        };
        let defaults = AccessDefaults::default();
        defaults.configure(key.host.clone(), &config).unwrap();
        (authority, key, config, defaults)
    }

    #[test]
    fn verification_for_a_closed_connection_or_old_configuration_is_discarded() {
        let (authority, key, config, defaults) = trust_fixture();
        let token = issue_trust_token(&authority, "eng", &key.peer.to_string(), None).unwrap();
        let tokens = std::slice::from_ref(&token);

        // The connection closes, and the peer reconnects, while tokens are verified.
        defaults.peer_connected(&key.peer);
        let verified = defaults.verify(&key.peer, tokens).unwrap();
        defaults.peer_disconnected(&key.peer);
        defaults.peer_connected(&key.peer);
        assert!(defaults.commit(&key.peer, verified).is_empty());
        assert_eq!(defaults.resolve(&key).0, PeerGrant::None);

        // The host is reconfigured while tokens are verified.
        let verified = defaults.verify(&key.peer, tokens).unwrap();
        let mut other_authority = config.clone();
        other_authority.trust[0].authority = libp2p::PeerId::random().to_string();
        defaults
            .configure(key.host.clone(), &other_authority)
            .unwrap();
        assert!(defaults.commit(&key.peer, verified).is_empty());
        assert_eq!(defaults.resolve(&key).0, PeerGrant::None);

        // Without interference the verification is committed.
        defaults.configure(key.host.clone(), &config).unwrap();
        let verified = defaults.verify(&key.peer, tokens).unwrap();
        assert_eq!(defaults.commit(&key.peer, verified), ["eng"]);
        assert_eq!(defaults.resolve(&key).0, PeerGrant::Full);
    }

    #[test]
    fn changing_the_host_drops_connections_tracked_for_the_old_one() {
        let defaults = AccessDefaults::default();
        let peer = NodeId::from_peer_id(libp2p::PeerId::random());
        let config = MeshAccessConfig {
            lan: PeerGrant::Full,
            trust: vec![],
        };
        let first = NodeId::from_peer_id(libp2p::PeerId::random());
        defaults.configure(first.clone(), &config).unwrap();
        defaults.lan_connection_opened(&peer);
        // Reconfiguring the same host keeps live connections.
        defaults.configure(first.clone(), &config).unwrap();
        let key = |host: &NodeId| PeerGrantKey {
            host: host.clone(),
            peer: peer.clone(),
        };
        assert_eq!(defaults.resolve(&key(&first)).0, PeerGrant::Full);
        let second = NodeId::from_peer_id(libp2p::PeerId::random());
        defaults.configure(second.clone(), &config).unwrap();
        assert_eq!(defaults.resolve(&key(&second)).0, PeerGrant::None);
    }

    #[tokio::test]
    async fn each_peer_has_at_most_one_expiry_timer_while_connected() {
        let (authority, key, _, defaults) = trust_fixture();
        let short = issue_trust_token(&authority, "eng", &key.peer.to_string(), Some(60)).unwrap();
        let long = issue_trust_token(&authority, "eng", &key.peer.to_string(), Some(120)).unwrap();
        let timers = || defaults.0.state.read().expiry_timers.len();
        for _ in 0..3 {
            defaults.peer_connected(&key.peer);
            for token in [&short, &long, &short] {
                defaults.present(&key.peer, std::slice::from_ref(token));
                assert_eq!(timers(), 1);
            }
            defaults.peer_disconnected(&key.peer);
            assert_eq!(timers(), 0);
        }
        // A token without expiry needs no timer.
        let forever = issue_trust_token(&authority, "eng", &key.peer.to_string(), None).unwrap();
        defaults.peer_connected(&key.peer);
        defaults.present(&key.peer, &[forever]);
        assert_eq!(timers(), 0);
    }

    #[test]
    fn next_expiry_ignores_passed_and_unlimited_memberships() {
        let mut state = State::default();
        let peer = NodeId::from_peer_id(libp2p::PeerId::random());
        state.members.insert(
            peer.clone(),
            BTreeMap::from([("a".into(), 0), ("b".into(), 100), ("c".into(), 200)]),
        );
        assert_eq!(state.next_expiry(&peer, 50), Some(100));
        assert_eq!(state.next_expiry(&peer, 100), Some(100));
        assert_eq!(state.next_expiry(&peer, 101), Some(200));
        assert_eq!(state.next_expiry(&peer, 201), None);
    }
}
