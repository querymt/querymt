## ADDED Requirements

### Requirement: Mesh grants are host-owned and directional

The resource host SHALL keep one durable grant per transport-authenticated peer across its mesh scopes. Membership, claimed identity, and actor references SHALL NOT confer access. Peers without a grant, including on upgrade, SHALL get identity-only discovery. If the policy store is unavailable, protected requests SHALL fail closed.

#### Scenario: An admitted peer has no grant
- **WHEN** a LAN or invited peer without a grant discovers the host and requests sessions or providers
- **THEN** it can see the node's identity but receives no protected data and cannot invoke protected operations

#### Scenario: Grant direction and identity do not change across scopes
- **WHEN** a host grants a peer access, that peer reconnects under a different name or scope, and another peer claims the same identity in a request field
- **THEN** only the authenticated grantee receives access; no reverse grant or grant for the other peer is created

### Requirement: Mesh state stores peer access alongside membership

The host SHALL store grants, quota caps, and settled usage in the peer-access section of `mesh_state.json` version 2. A single in-process owner SHALL serialize membership and peer-access writes. Saves SHALL write and sync a temporary file in the same directory before renaming it in place. Older builds saving `mesh_state.json` drop the peer-access section.

#### Scenario: Version 1 mesh state upgrades without granting access
- **WHEN** the host loads version 1 mesh state or starts without an existing file
- **THEN** it uses version 2 with empty peer-access state and preserves any existing membership and invite records

### Requirement: The host manages sharing through ACP

The host SHALL expose operations through ACP to read and replace grants and inspect and reset quotas. The mesh peer protocol SHALL NOT expose these operations, even under a Full access grant. Policy writes SHALL validate the entire grant, persist it before reporting success, and reject edits based on stale revisions rather than overwriting a newer grant.

#### Scenario: A peer cannot edit grants
- **WHEN** a peer with Full access attempts a grant or quota reset over the mesh protocol
- **THEN** the host refuses it and no policy or accounting state changes

#### Scenario: A stale ACP edit is refused
- **WHEN** an ACP client submits an edit based on a revision that another client already replaced
- **THEN** the host reports a conflict and retains the persisted policy

### Requirement: Session and filesystem sharing have separate controls

Sessions **Off** SHALL block all remote session operations, even through prior references. Sessions **On** SHALL allow host-wide session access subject to other grants. Filesystem **Off** SHALL block fresh workspace access. Filesystem **On** SHALL require Sessions **On** and preserve workspace confinement. Except for explicitly enabled Shell, Custom sessions SHALL deny tools or actions whose capabilities the host cannot enforce, at execution as well as discovery.

#### Scenario: Sessions off blocks an existing reference
- **WHEN** a peer with Sessions Off calls a session it can identify using a prior remote reference
- **THEN** the host exposes no session list, history, or runtime, and refuses create, attach, resume, fork, subscriptions, prompts, interactions, and schedules

#### Scenario: Session sharing does not enable filesystem access
- **WHEN** a peer has Sessions On and Filesystem Off and opens a session or requests file access through the session
- **THEN** allowed session data, including artifacts already saved in history, is available, but fresh file reads, writes, indexes and filesystem-capable tools or hooks are denied

#### Scenario: Filesystem sharing preserves workspace confinement
- **WHEN** a peer with both toggles On uses a supported workspace file operation against an in-workspace path and a path outside that workspace
- **THEN** the in-workspace operation proceeds subject to normal host rules and the out-of-workspace operation is refused

#### Scenario: A Custom peer creates or forks a session
- **WHEN** a Custom peer with Sessions On uses the grant-aware protocol to create a new session or fork saved history at a message in that session
- **THEN** the host SHALL create a session without a workspace, reject a caller-selected cwd, and commit the authenticated origin with the new session and any copied history
- **AND** the host SHALL NOT inherit the source checkout, allocate a workspace, or start integrations it cannot confine; a later load SHALL NOT substitute a workspace or attach such integrations

### Requirement: Custom grants have an explicit Shell control

Shell: allows the unsandboxed shell. In Custom grants, Shell SHALL default to Off. Shell **On** SHALL require Filesystem **On**. Shell **Off** SHALL refuse shell execution through the session.

#### Scenario: Custom shell follows the grant
- **WHEN** a Custom peer requests shell execution with Shell On or Off
- **THEN** the host applies the Shell control at discovery and execution, subject to existing tool policy

#### Scenario: Invalid Custom combinations are refused
- **WHEN** an owner submits Shell On without Filesystem On or Filesystem On without Sessions On
- **THEN** the host reports an invalid grant without changing access

### Requirement: Full access is explicit and does not prompt for mesh approval

Full access SHALL grant sessions, filesystem, shell, and all providers and models with unlimited capacity. Standing grants SHALL NOT add per-request mesh approvals or override existing host approvals, denials, or agent questions.

#### Scenario: Full access does not add approval prompts
- **WHEN** a Full access peer uses a shareable operation that the host's existing policies already allow without a prompt
- **THEN** it proceeds without a mesh approval prompt, and an existing host always-ask rule still applies to operations it covers

### Requirement: Access changes apply to existing peer work

Each protected operation, including deferred work after restart, SHALL use the current grant of its authenticated initiator. Revocation SHALL stop affected delivery and cancel affected work at the next supported boundary without undoing committed effects or interrupting unrelated work. Peers SHALL NOT take over more privileged execution or supply another peer's callback.

#### Scenario: Revocation stops an attached peer without disconnecting others
- **WHEN** an owner turns off Sessions for an attached peer with live events, queued input, and a scheduled turn
- **THEN** that peer receives no new live events or remote session access, the queued or scheduled work does not run, and local and other authorized peers retain access

#### Scenario: A restricted peer cannot control a more privileged run
- **WHEN** a restricted peer tries to steer an owner-initiated run or resolve an interaction outside the peer's authority
- **THEN** the host refuses it without applying the input or changing the run's authority

### Requirement: Scheduled work keeps its origin across restart

A host SHALL record authenticated peer origin before accepting a new remote schedule, and SHALL reject a new remote schedule request with no authenticated origin. Schedules without a stored origin SHALL run as Local for compatibility. When a peer schedule becomes due before the host policy store is initialized, the host SHALL wait or retry without counting this as a denied run.

#### Scenario: Custom session sharing enables peer-owned schedules
- **WHEN** a peer with Sessions On uses the grant-aware protocol to create, list, pause, resume, trigger, or delete a schedule
- **THEN** it can schedule work on a shared session and manage only schedules owned by its authenticated host/peer pair, regardless of the protocol used at creation
- **AND** the scheduler SHALL check the requesting authority before applying a queued mutation, without replacing the schedule's stored execution origin

#### Scenario: Origin-less schedules still run after restart
- **WHEN** a host restarts with an existing schedule that has no stored origin
- **THEN** the schedule runs as Local using the usual scheduler behavior

#### Scenario: A remote schedule cannot be created without a peer origin
- **WHEN** a new remote schedule request lacks authenticated peer origin
- **THEN** the host rejects creation without persisting an unusable schedule

#### Scenario: A peer schedule becomes due during startup
- **WHEN** a persisted peer schedule becomes due before the host policy store is ready
- **THEN** the host waits or retries and applies the peer's current grant when it is ready

### Requirement: Discovery reports access in the right direction

The owner view SHALL distinguish what this host shares from what the selected peer shares in return. Peer-facing data SHALL be limited to the caller's effective grant and permitted metadata. The service SHALL distinguish denied, unavailable, and incompatible peers, invalidate known stale access, and never infer access from a legacy capability badge.

#### Scenario: Nodes shows asymmetric access
- **WHEN** one node shares only a provider and the other shares only sessions in return
- **THEN** each owner view reports both directions separately, the provider-only recipient sees no remote session count, and neither recipient sees third-party grants or quota state

### Requirement: Upgraded clients can consume legacy sharing without granting access

An upgraded client SHALL support existing shared resources of a host whose legacy protocol is confirmed through authenticated negotiation. It SHALL report this as legacy sharing without granular controls, not as a Full access grant. It SHALL NOT create a reverse grant or select legacy mode after a denial, timeout, or failure to verify the host's protocol.

#### Scenario: Legacy sharing does not create a reverse grant
- **WHEN** an upgraded client connects to a confirmed legacy host and uses a session or provider that host shares
- **THEN** the supported operation proceeds, Nodes labels the remote sharing as legacy, and the upgraded client grants no access to its own resources

#### Scenario: Failure does not trigger a legacy downgrade
- **WHEN** an upgraded host denies a request or protocol negotiation times out or cannot be verified
- **THEN** the client reports the denial or negotiation failure and does not retry via a legacy protocol

### Requirement: Legacy requests to upgraded hosts require explicit Full access

An upgraded host SHALL serve protected requests in a legacy wire format only for peers with an explicit current Full access grant, identified by the authenticated transport. It SHALL apply the same authorization and revocation rules as for upgraded clients. Custom grants SHALL require the new sharing protocol. A legacy operation the host cannot authorize SHALL be refused even under Full access.

#### Scenario: Legacy client needs explicit Full access
- **WHEN** a legacy client requests sessions, workspace files, or models on an upgraded host before and after its owner grants that peer Full access
- **THEN** the host exposes only identity before the grant, and after the grant serves supported protected requests from that peer without requiring a client upgrade

#### Scenario: Legacy wire format does not satisfy a Custom grant
- **WHEN** a client with a Custom grant sends a protected legacy request or a Full access client sends a legacy operation the host cannot authorize
- **THEN** the host refuses the operation without returning protected data or doing upstream work

#### Scenario: Legacy sharing stops when Full access is removed
- **WHEN** the upgraded host replaces a legacy client's Full grant with Custom or No access during an active session subscription or provider stream
- **THEN** new protected legacy requests are refused, existing protected delivery stops, and affected work is cancelled at the next supported boundary
