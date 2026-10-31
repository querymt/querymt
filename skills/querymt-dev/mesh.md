# Mesh and remote-session checks

Read [runtime isolation](runtime.md) and [ACP checks](acp.md) first. Mesh tests need
mesh: do not replace them with local-only fixtures or cancel before the affected
remote inference. Separate provider placement, session ownership and the client
connection; they can be on different nodes.

## Select the topology

- Use separated processes for disconnect/lifetime behavior. In-process actors can
  share a swarm/runtime and fail to reproduce actual remote semantics. Use two
  hosts when the bug requires that boundary.
- Give each test node its own HOME/config/DB/identity/ports and unique job name.
  Use a test scope to avoid unrelated peers. For an Iroh test pair, discover the
  current invite/join flags, create a test invite and disable
  LAN discovery with `--mesh-no-lan`. LAN-specific tests instead need controlled
  LAN discovery; do not disable the transport being tested.
- Verify discovery includes the intended peers and record scope/node IDs.
  A restarted observer reuses its own saved test state,
  not a new one-use join token. Use explicit fixture profiles with distinguishable
  owner/observer model settings to prove which configuration controls execution.
- Record source/dirty-patch identity and the actual binary on each host; matching
  source does not imply identical host binaries. Preserve remote worktree changes.

## Run only the relevant gate

| Failure class | Gate |
|---|---|
| Startup/delegation routing | Match real order: build profiles, attach mesh, create session, actually infer/delegate; assert provider node and child result |
| Observer loss | Hold owner generation deterministically; stop only the observer created for this test, never a user-run instance; verify owner PID/run/start/actor continuity, reconnect and recover completion without resubmission |
| Question replay | Hold unanswered interaction; reconnect observer; match interaction/session IDs and new route, answer exactly once, assert owner result/history |
| Queue/steer | Submit against a held owner run, match input IDs and states to provider requests/history, then verify idle/drained runtime |
| Discovery/admission | Verify scoped visibility, no cross-scope leakage, persistence/readmission and bounded discovery/catalog progress |

Record bounded timing and transient errors, not just eventual success. A delivery
RPC accepting a rescan is not proof the client received the question. Owner crash,
observer crash, relay termination, lost acknowledgement and physical partition are
different scenarios; do not label one as all of them. A passing replay gate does
not prove durable input deduplication or permission enforcement.

Use the existing client skill to check whether the required interaction and
connection behavior is supported. A missing client capability must be reported,
not replaced by a weaker assertion. When no remote test host is available, finish
local process/contract checks and report which cross-host behavior remains unproved.
