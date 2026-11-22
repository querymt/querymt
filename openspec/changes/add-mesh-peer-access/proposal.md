# Proposal: Per-peer mesh sharing

## Why

Connecting to a QueryMT mesh currently exposes the same node capabilities to its peers. The Nodes badges describe what the host can do, not what it shares with you. An owner needs to lend a specific provider and model without also lending their sessions, workspace, shell, or unlimited inference capacity.

## What Changes

- Give each host a durable grant for each authenticated peer: **No access**, **Custom**, or **Full access**. Grants are directional, apply across mesh scopes on that host, and can be changed without reconnecting.
- In Custom, expose session and workspace filesystem toggles and selected providers, each with all models or only explicitly selected models. Show the shell toggle, but require Full access for the current unsandboxed shell. Reject combinations the host cannot safely enforce; do not silently turn on other permissions.
- Give each peer an independent allowance per provider and per model: unlimited or a finite amount in millions of tokens (Mtok), reset manually or on a daily, weekly, or monthly schedule. Requests must fit every applicable cap. The same caps apply to direct inference and inference triggered through hosted sessions.
- Apply grants on the resource owner, not just in discovery. Revoking access stops new use through existing sessions, subscriptions, and streams. Already delivered data is not recalled.
- Expose grant management over ACP and a separate peer-facing view of effective inbound access. Full access grants all shareable resources, including unlimited providers, with no new mesh approval prompts. Existing agent tool approvals and agent questions are unchanged.
- **BREAKING default, mixed-version compatible**: Upgrade does not convert existing memberships into grants. Upgraded nodes can still use what legacy hosts already share. Legacy clients can use supported operations on an upgraded host only after its owner explicitly grants their authenticated peer Full access. Custom grants require the new sharing protocol; no denial or timeout triggers a downgrade.

## Non-goals

Per-session and per-directory grants; a sandbox for restricted shell; rolling windows, custom time zones, stacking multiple windows on the same quota scope, money limits, and provider bill reconciliation; recall of copies peers already received; Custom grants over the legacy protocol; changing local approval policy or automating answers to agent questions; ACP authentication, which is planned separately.

## Capabilities

### New Capabilities

- `mesh-peer-access`: authenticated directional grants, session and tool enforcement, live revocation, and owner/peer access reporting.
- `mesh-provider-quotas`: all-or-selected model grants, stacked provider and model caps, recurring UTC windows, durable reservations, and usage settlement.
- `mesh-access-acp`: discoverable ACP methods for editing grants and quotas, reading both sharing directions, and receiving invalidations.

### Modified Capabilities

- `remote-session-sync`: preserve history transfer from legacy hosts and distinguish access denial from offline or format fallback.
- `remote-stream-relay`: preserve mixed-version streams without granting access to upgraded-host resources through relay compatibility.

## Impact

- `querymt-remote` (`kameo-mesh`): **BREAKING** public Rust mesh and `ProviderShare` setup must supply policy context and a store to enable sharing. Add a versioned sharing capability and typed errors; preserve supported legacy wire formats through authorized entry points, not implicit trust.
- `querymt-agent-runtime` (`remote`, `api`): **BREAKING** ACP Nodes/remote access semantics (including optional `active_sessions`), new v1 sharing methods and `peerAccessChanged` notification, and policy enforcement in remote session execution, tool/hook dispatch and model routing. Local sessions keep their existing behavior. No new settings in agent TOML/JSON.
- `querymt-agent-sqlite` (proposed opt-in `mesh-access`, off by default): a separate host policy/quota store; no changes or migration to the session DB. `querymt-agent` (`remote`, `dashboard`): production store and ACP control wiring. Mobile/Python agent consumers inherit the new remote behavior where enabled; no new language-specific public method is required.
- Regenerate affected typeshare DTOs. `querymt-desktop` follow-up: distinguish **What I share** from **What this peer shares with me**, edit grants, show both quota scopes with usage and next reset times, and distinguish legacy sharing without granular controls from denied, unavailable, or incompatible states. No Extism contract or provider plugin API changes are proposed.
