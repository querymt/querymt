# Spec Delta

## ADDED Requirements

### Requirement: Skills are listed from servers that declare the skills extension
WHEN an MCP server connected to a session declares the skills extension and its name contains no `.`, THEN the system SHALL list all of its skills during connection, following pagination. WHEN a server does not declare the extension, THEN the system SHALL NOT send it any skills request. This SHALL apply to servers from agent configuration, `.agents/mcp.json`, ACP clients and preconnected peers.

#### Scenario: Server declares skills
- **WHEN** a session connects to a server whose capabilities declare `io.modelcontextprotocol/skills`
- **THEN** the server's skills are listed and become available to that session's skill tool

#### Scenario: Server without the extension
- **WHEN** a session connects to a server that does not declare the skills extension
- **THEN** no `skills/list` request is sent to it and its tools are available as before

#### Scenario: Client-supplied server
- **WHEN** an ACP client supplies an MCP server that declares the skills extension for a new session
- **THEN** that server's skills are available in that session only

### Requirement: Servers with dotted names are not asked for skills
WHEN a server declaring the skills extension has a name containing `.`, THEN the system SHALL NOT list its skills, SHALL report a diagnostic naming the server, and SHALL keep its tools available.

#### Scenario: Server name with a dot
- **WHEN** a session connects to a server named `team.docs` that declares the skills extension
- **THEN** no `skills/list` request is sent to it, a warning names the server, and its tools are available as before

### Requirement: Skill listing failures do not fail the connection
WHEN listing a server's skills fails, or the listing contains invalid entries, THEN the server SHALL remain connected with its tools available. The failure SHALL be reported as a diagnostic naming the server, and that server SHALL contribute only its valid skills, or none if listing failed.

#### Scenario: Skills listing errors
- **WHEN** a server declares the skills extension but its `skills/list` request fails
- **THEN** the session is created, the server's tools are available, and a warning names the server

### Requirement: Server skills are refreshed on resource list changes
WHEN a connected server whose skills are listed announces that its resource list changed, THEN the system SHALL list its skills again and replace only that server's skills in the session. This SHALL hold for servers using the 2026-07-28 protocol. Other servers' tools and skills SHALL remain unchanged. A failed refresh SHALL keep the server's previously listed skills.

#### Scenario: Resource list changed
- **WHEN** a server announces `notifications/resources/list_changed` after publishing a new skill
- **THEN** the session's skills from that server are replaced with the newly listed set
- **AND** skills from other servers are unchanged

#### Scenario: Server on the 2026-07-28 protocol
- **WHEN** a server that negotiated protocol version 2026-07-28 publishes a new skill and announces that its resource list changed
- **THEN** the session's skills from that server include the new skill

#### Scenario: Refresh fails
- **WHEN** a server announces a resource list change and the following skills listing fails
- **THEN** the session keeps that server's previously listed skills and a warning names the server

### Requirement: Concurrent MCP list refreshes do not lose updates
WHEN refreshes of tools and skills for one or more servers of a session complete at the same time, THEN the session's MCP state SHALL reflect every completed refresh, and each refresh SHALL replace only the part it fetched for its own server.

#### Scenario: Tool and skill refreshes overlap
- **WHEN** server `docs` announces a tool list change and a resource list change and both refreshes complete concurrently
- **THEN** the session's MCP state contains both the refreshed `docs` tools and the refreshed `docs` skills
