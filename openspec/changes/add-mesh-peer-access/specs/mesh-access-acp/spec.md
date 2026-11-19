## ADDED Requirements

### Requirement: Clients discover mesh access controls on their ACP connection

`querymt/capabilities` SHALL advertise `mesh_access_version: 1` and the mesh access method and notification names only on ACP connections authorized to administer the local host. An owner-capable ACP stdio connection SHALL support v1. WebSocket SHALL support v1 only when the server verifies owner authority for that connection. These methods SHALL be host-scoped, not session- or profile-scoped, and SHALL NOT forward administration to a mesh peer.

#### Scenario: ACP advertises sharing controls only to the host owner
- **WHEN** an owner connects over an owner-controlled stdio path or an owner-authenticated WebSocket, and another client connects without owner authority
- **THEN** the owner can discover the v1 contract before loading a session; the other connection does not receive the advertisement, and a direct call to a sharing method is refused without disclosing grants or usage

### Requirement: ACP lists peers and reads directional sharing state

`querymt/mesh/nodes` SHALL remain the discovery entry point. On a v1 owner connection it SHALL include the peer IDs of saved sharing policies, including offline peers. `querymt/mesh/getPeerAccess` SHALL return the local grant, revision and quota status, the local shareable model choices, and the remote peer's effective grant and quota status only when the peer makes those details available. A failed remote lookup SHALL NOT hide the local policy.

#### Scenario: ACP separates saved local policy from unavailable remote access
- **WHEN** a previously granted peer goes offline and the owner opens the peer's sharing settings without an attached session
- **THEN** the peer remains discoverable for grant management, the local policy and quota status stay editable, and the remote side is marked unavailable rather than showing a stale positive grant

### Requirement: ACP changes grants and quotas with confirmed revisions

`querymt/mesh/setPeerAccess` SHALL replace the local access grant; `querymt/mesh/setPeerQuota` SHALL edit or explicitly reconfigure a quota; `querymt/mesh/resetPeerQuota` SHALL reset one finite quota. Each mutation SHALL require the revision from the local state, persist changes before returning the confirmed state, and leave state unchanged on rejection. It SHALL NOT require a reachable peer. Changing access SHALL NOT reset saved quotas.

#### Scenario: Owner manages sharing and quotas entirely through ACP
- **WHEN** the owner sets a provider or model cap, saves a Custom grant with selected models, resets one cap, and later changes the grant to No access using the returned revisions
- **THEN** each result reflects the committed local state, only the targeted cap resets, and No access revokes sharing without erasing saved quota state

#### Scenario: Stale and blocked ACP mutations are distinguishable
- **WHEN** an owner writes with a stale revision or requests a reset while attempts are outstanding
- **THEN** ACP returns a distinct machine-readable error for each case and neither request changes the grant or quota

### Requirement: ACP sends a peer-scoped sharing invalidation

`querymt/mesh/peerAccessChanged` SHALL be a server-to-client invalidation identifying the affected peer and direction, not a state snapshot. It SHALL notify authorized owner connections of observed changes to policy, quota status, availability or shareable models. Mutations SHALL notify only after commit. Delivery MAY be coalesced and SHALL NOT require an attached session. Reads SHALL remain authoritative after missed notifications or reconnect.

#### Scenario: A second ACP client refreshes after sharing changes
- **WHEN** one owner client updates a grant or quota while another owner client is connected
- **THEN** the second client receives an outgoing invalidation, can read the committed state, and a client without owner authority receives no invalidation

#### Scenario: ACP recovers current peer access without notification replay
- **WHEN** an owner client misses an incoming change while disconnected and reconnects
- **THEN** it can re-read the peer's current access and quota status without replay and does not retain the old grant as current

### Requirement: Mesh access ACP v1 has explicit JSON shapes

Requests, results, notifications and errors SHALL use the v1 wire contract below. New requests SHALL include `version: 1`; unsupported versions and invalid fields SHALL be rejected without a mutation.

#### Scenario: ACP wire payloads round trip for provider and model quotas
- **WHEN** a client reads the local catalog, writes an exact finite amount and recurring window for a model, and reads it back
- **THEN** the advertised methods use the specified request and result shapes, the amount and cadence round trip without rounding, and an unsupported version is rejected without a mutation

##### V1 Wire Contract

Object fields use snake_case. The advertised method names are shown below. On the wire, clients use the ACP extension request prefix (`_querymt/...`); the WebSocket dispatcher also accepts unprefixed requests. The notification uses the unprefixed name.

| Method | Parameters in addition to `version` | Result |
| --- | --- | --- |
| `querymt/mesh/getPeerAccess` | `peer_id` | `{version, peer_id, outgoing, incoming, shareable_catalog}` |
| `querymt/mesh/setPeerAccess` | `peer_id`, `expected_revision`, `grant` | `{version, peer_id, outgoing}` |
| `querymt/mesh/setPeerQuota` | `peer_id`, `expected_revision`, `scope`, `operation`, `limit` | `{version, peer_id, outgoing}` |
| `querymt/mesh/resetPeerQuota` | `peer_id`, `expected_revision`, `scope` | `{version, peer_id, outgoing}` |

- `peer_id` is the canonical mesh peer ID string, not the node label. It matches `nodes[].id` for mesh peers. The existing `querymt/mesh/nodes` result adds `managed_peer_ids: string[]` on v1 owner connections; it lists peers with saved grants or quota configuration, including offline peers. `nodes[].active_sessions` is omitted when not authorized or known. It is not reported as zero in place of a denial.
- `outgoing` is `{revision, grant, quotas}`. `revision` is an opaque string that also exists for a default No access grant. The client echoes it as `expected_revision` on every mutation. It changes on committed grant/configuration changes and owner resets, not on usage or automatic rollover. A rejected edit does not advance it. Mutation results never depend on a remote lookup.
- A stored `grant` is `{"mode":"none"}`, `{"mode":"full"}`, or `{"mode":"custom","sessions":bool,"filesystem":bool,"providers":[...]}`. Custom cannot enable shell. Each provider selection is `{provider_id, models}`, where `models` is `{"mode":"all"}` or `{"mode":"selected","model_ids":[...]}` with a non-empty canonical model ID list. The provider list may be empty; omitted providers are not shared. `setPeerAccess` replaces the selection, not quota configuration. A UI presenting Full access must warn about unsandboxed shell as specified by the sharing policy.
- `scope` is `{provider_id}` for a provider or `{provider_id, model_id}` for a single model. A `limit` is `{"kind":"unlimited"}` or `{"kind":"finite","limit_tokens":"2000000","window":"daily"}`. `window` is `manual`, `daily`, `weekly`, or `monthly`, with `manual` the default when omitted on input. Token counters and limits are exact integer decimal strings; the UI presents them in Mtok. `operation: "update"` changes only the amount of an existing finite quota without a reset; `operation: "reconfigure"` explicitly changes finite/unlimited state or cadence (including adding the first finite cap). Owners can preconfigure quotas before granting access; doing so does not grant access.
- Each entry in `quotas` is `{scope, limit, active, usage}`. `active` distinguishes a quota currently applied to Custom access from a saved quota not currently applied. `usage` contains `known_tokens`, `estimated_tokens` and `has_unknown_usage`; for a finite entry, `usage` also contains `window_started_at`, `reserved_tokens`, and `available_tokens`, plus `next_reset_at` for recurring windows. UTC times are RFC 3339 strings. The host derives status at read time; the client does not compute the authoritative remaining balance.
- `incoming` has a `status` of `granted`, `denied`, `unavailable`, `incompatible`, or `legacy`. Only `granted` includes `effective` (`{mode, sessions, filesystem, shell, providers}` with provider selections as above, or all host providers represented by their allowed selections for Full) and `quotas` (only scopes disclosed to this peer). `legacy` does not synthesize a verified Full grant or quota data. `shareable_catalog` lists local `{provider_id, models}` entries; each catalog model is `{model_id, finite_quota_supported}`. No credentials, endpoints, or other peers' policies appear in these results.

The notification's `params` are `{version: 1, peer_id, direction, reason}`. `direction` is `outgoing` for the local host's policy toward the peer or `incoming` for the peer's sharing with this host. `reason` is `policy`, `quota`, `availability`, or `catalog`. Notifications carry no grant contents or usage. A client SHALL read back after invalidation or reconnect; it MAY also refresh on focus or at `usage.next_reset_at`, since calendar rollover need not produce a notification without host activity. The existing `querymt/mesh/nodesChanged` continues to signal topology changes.

ACP errors SHALL be distinguishable by JSON-RPC error `data.code`, not message text. Invalid input uses `-32602` with `mesh_access_invalid_params`; unimplemented methods use `-32601`. Domain failures use `-32000` with one of the following codes:

| `data.code` | Meaning or additional fields |
| --- | --- |
| `mesh_access_owner_required` | Local owner authority missing; no policy details. |
| `mesh_access_unavailable` | No working local sharing authority or store. |
| `mesh_access_conflict` | Stale write; `expected_revision`, `actual_revision`. |
| `mesh_quota_reconfigure_required` | An amount-only edit cannot change the quota kind or cadence. |
| `mesh_quota_in_flight` | A quota change that starts a fresh period is blocked; `scope`. |
| `mesh_access_denied` | An operation is not allowed by the current grant. |
| `mesh_quota_exhausted` | An operation did not fit; `limits` is an array of `{scope, available_tokens, next_reset_at?}` entries for the limiting scopes. |
| `mesh_quota_unsupported` | An allowed binding cannot enforce the required finite bound; `scope` identifies an applicable quota. |
