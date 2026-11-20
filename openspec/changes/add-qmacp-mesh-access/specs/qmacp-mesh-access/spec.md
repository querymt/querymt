## ADDED Requirements

### Requirement: qmacp discovers mesh management support

`qmacp mesh nodes` SHALL read the advertised node listing method. `qmacp mesh get PEER_ID` and writes SHALL require mesh access v1 and the specific advertised method. Reads SHALL report the server's JSON, including unavailable, denied and legacy states without treating them as verified grants.

#### Scenario: Mesh commands do not assume owner support
- **WHEN** a server advertises node discovery but not mesh access v1
- **THEN** `mesh nodes` can read discovery and `mesh get` and mutations fail without sending a management request

### Requirement: Mesh writes are explicit and revision checked

`mesh set PEER_ID --expected-revision REV --grant JSON` SHALL send an explicit grant replacement. Full requires `--allow-host-shell`. `mesh set-quota` SHALL take a provider ID, optional model ID, a limit and update or reconfigure intent. `mesh reset-quota` SHALL target the supplied scope. Each write SHALL require an explicit revision and return the confirmed state without automatic retry.

#### Scenario: Mesh grant edits require an explicit revision and Full acknowledgement
- **WHEN** a client requests a Full grant with and without its acknowledgement, or omits the revision
- **THEN** only a correctly acknowledged, revision-bearing command sends the Full replacement

#### Scenario: Mesh quota requests retain exact scoped limits
- **WHEN** the owner submits a model limit of `2.000001` Mtok and a daily window, then requests a reset for that scope and revision
- **THEN** the client sends `2000001` whole tokens and a model scope, and the reset targets only that scope

#### Scenario: Mesh mutation rejection is not automatically retried
- **WHEN** the server rejects a write or disconnects without a reply
- **THEN** qmacp returns nonzero with JSON error and peer/revision context, reporting a known server rejection or an unknown outcome without retrying the write

### Requirement: qmacp can wait for a scoped sharing invalidation

`qmacp mesh watch PEER_ID --timeout SECONDS` SHALL require the v1 notification capability and return one matching invalidation without loading a session. The wait SHALL be bounded and return the existing timeout exit code if no matching hint arrives. The hint SHALL NOT be represented as authoritative state.

#### Scenario: Mesh watch filters notifications without a session
- **WHEN** the server sends another peer's notification and then the requested peer's valid v1 notification
- **THEN** qmacp returns only the matching hint and sends no session load request
