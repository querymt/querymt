## MODIFIED Requirements

### Requirement: Older owners remain supported

Within an established sharing protocol, the attaching agent SHALL use paged transfer if the owner permits the load but does not support chunked transfer, and remember that limitation. This applies to upgraded and confirmed legacy owners. A paged sync replaces the local copy with a full transfer; it does not support incremental, tail-first, or epoch-based transfer. A denial or timeout SHALL NOT trigger format fallback or a legacy downgrade.

#### Scenario: Owner on an older version
- **WHEN** a confirmed legacy owner shares the requested session and does not support chunked transfer
- **THEN** the attaching agent loads the events using the legacy paged protocol without granting reverse access

#### Scenario: Second open from the same older owner
- **WHEN** another allowed session is opened from the same paged-only owner using the same negotiated protocol
- **THEN** the attaching agent uses paged requests without first sending a chunked request

### Requirement: Incomplete transfer is reported, not hidden

If an authorized load exceeds its transfer budget, the agent SHALL return a typed, retryable remote-transfer error, never partial history. If the owner is unreachable, the agent MAY serve a complete local copy marked stale unless a known denial applies. A confirmed denial SHALL return an access error, with neither transfer fallback nor cache fallback. This does not recall copies already delivered to the peer.

#### Scenario: Owner becomes unreachable mid-transfer
- **WHEN** the owner stops responding before the recent window has been transferred
- **THEN** the load fails with a retryable remote-transfer error rather than returning partial history

#### Scenario: Owner offline, window already local
- **WHEN** the owner is unreachable and the permitted window was transferred in full with no known denial since
- **THEN** a load served from the local copy is explicitly marked cached and possibly out of date

#### Scenario: Confirmed denial cannot fall back to cached history
- **WHEN** an owner refuses a session load after it was previously cached
- **THEN** the attaching agent returns an access error and does not satisfy the load from cache even if the owner subsequently goes offline
