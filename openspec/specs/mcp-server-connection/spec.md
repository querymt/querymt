# mcp-server-connection Specification

## Purpose

Defines how QueryMT connects to configured MCP servers and keeps those connections usable: authorization of HTTP servers, credential lifecycle, response caching of server listings, transport selection, and validation of server settings.

## Requirements

### Requirement: HTTP MCP servers can authorize with OAuth
An HTTP MCP server declared in the agent configuration with OAuth authorization SHALL be connected using credentials obtained through the MCP authorization flow (server metadata discovery, client registration or a configured client id, PKCE). The system SHALL attach the resulting access token to requests and SHALL refresh it before or upon expiry without user involvement.

#### Scenario: Stored credentials are valid
- **WHEN** a session starts with an OAuth-configured MCP server for which valid credentials are stored
- **THEN** the server connects and its tools are available without user interaction

#### Scenario: Access token expired
- **WHEN** the stored access token has expired and a refresh token is stored
- **THEN** the system refreshes the token, stores the new credentials, and the connection succeeds

#### Scenario: Static headers still work
- **WHEN** an HTTP MCP server is configured with headers and without OAuth
- **THEN** the server is connected with those headers exactly as before

### Requirement: OAuth applies only to agent-configured servers
The system SHALL apply OAuth to a session's MCP server only when the agent configuration declares an OAuth HTTP server with the same name and the same URL, and SHALL take the OAuth settings from the agent configuration. A client-supplied server that requests OAuth without such a match SHALL fail to connect with an error naming the server. A client-supplied server that overrides an agent-configured OAuth server with a different URL SHALL connect without OAuth credentials.

#### Scenario: Client-supplied server requests OAuth
- **WHEN** an ACP client supplies an HTTP MCP server `other` with OAuth settings that is not declared in the agent configuration
- **THEN** the server fails to connect with an error stating that OAuth MCP servers must be declared in the agent configuration, naming `other`

#### Scenario: Override with a different URL
- **WHEN** an ACP client supplies a server named `linear` with a URL different from the agent-configured OAuth server `linear`
- **THEN** the server connects without the stored `linear` credentials

### Requirement: MCP servers needing authorization are skipped and reported
When an OAuth-configured MCP server has no usable credentials at connect time, or its authorization cannot be checked (for example the authorization server is unreachable), the system SHALL skip that server, SHALL continue the session with the remaining servers, and SHALL report the server, its auth provider id and, in the unreachable case, the error: through a notification for the session and in the `_meta` of the session creation/load response. The failure SHALL be logged. Other connection failures SHALL keep failing session setup.

#### Scenario: Authorization cannot be checked
- **WHEN** a session starts with an OAuth-configured server whose authorization server cannot be reached, and a second working server
- **THEN** the session starts with tools from the second server only
- **AND** the notification and the response `_meta` name the server and carry the error text

#### Scenario: No credentials yet
- **WHEN** a session starts with an OAuth-configured MCP server named `linear` and no stored credentials, and a second working server
- **THEN** the session starts with tools from the second server only
- **AND** clients receive a notification that `linear` requires authorization via provider `mcp:linear`
- **AND** the auth status for provider `mcp:linear` reports that login is required

#### Scenario: Other connection failure
- **WHEN** a session starts with a non-OAuth MCP server that fails to connect
- **THEN** session setup fails as before

### Requirement: Rejected refresh tokens require a new login
When a refresh token is rejected while a session is live, the system SHALL clear the stored credentials and SHALL fail that server's tool calls with an error naming the auth provider id to log in with.

#### Scenario: Refresh token rejected
- **WHEN** the authorization server rejects the stored refresh token for `linear` during a tool call
- **THEN** the stored credentials for `linear` are cleared and `mcp:linear` reports that login is required
- **AND** the tool call fails with an error telling the user to log in with provider `mcp:linear`

### Requirement: MCP OAuth login uses the existing auth flow
ACP clients SHALL be able to start, complete, query, and log out of MCP server authorization through the existing auth extension methods, using the provider id `mcp:<server-name>`. Login flows SHALL be scoped to the owner that started them. After a login completes, every session that connects its MCP servers afterwards SHALL be able to use that server's tools without restarting the agent. Sessions already running keep their existing connections.

#### Scenario: Login via loopback callback
- **WHEN** a client starts the auth flow for `mcp:linear` and the user approves in the browser
- **THEN** the callback is received, credentials are stored, and `mcp:linear` reports authenticated
- **AND** a session created afterwards can use tools from `linear`

#### Scenario: Flow owned by another client
- **WHEN** a client tries to complete an `mcp:linear` flow that another owner started
- **THEN** the request fails as if the flow did not exist

#### Scenario: Logout
- **WHEN** a client logs out of `mcp:linear`
- **THEN** stored credentials for `linear` are deleted and its status reports that login is required

#### Scenario: Unknown server
- **WHEN** a client starts the auth flow for `mcp:unknown` that is not an OAuth MCP server in the agent configuration
- **THEN** the request fails with an error naming the server

#### Scenario: Stable redirect URI
- **WHEN** an OAuth MCP server has no configured redirect port
- **THEN** the flow uses the same fixed loopback redirect URI on every login

### Requirement: MCP credentials are kept in the secret store and bound to the server URL
OAuth credentials for MCP servers SHALL be stored in the same secret store used for provider credentials, keyed per server name and bound to the server URL they were issued for. Credentials SHALL NOT be used for a server whose configured URL differs from the bound URL. They SHALL NOT be written to configuration files, logs, events, or session history.

#### Scenario: Credentials persisted across restarts
- **WHEN** the agent restarts after a successful MCP login
- **THEN** the server connects using the stored credentials without a new login

#### Scenario: Server URL changed
- **WHEN** the configured URL of `linear` changes after a login
- **THEN** the stored credentials are not sent to the new URL and `mcp:linear` reports that login is required

### Requirement: MCP list responses honour server cache hints
The system SHALL reuse a server's tool listing within the validity period the server declares and SHALL fetch it again when the server announces that its tool list changed. Caching SHALL be disabled, starting with the first listing, for a server whose configuration turns it off.

#### Scenario: List changed bypasses cache
- **WHEN** a server announces its tool list changed within the declared validity period of a cached listing
- **THEN** the system fetches the tool list from the server again and updates the available tools

#### Scenario: Cache disabled
- **WHEN** a server is configured with caching off
- **THEN** every tool listing, including the first, is fetched from the server

### Requirement: HTTP MCP servers can be reached over a Unix socket
On Unix platforms, an HTTP MCP server configured with a socket path SHALL be connected over that Unix domain socket, using the configured URL for request addressing and the `Host` header. On other platforms, such a configuration SHALL fail to connect with an error naming the server.

#### Scenario: Unix socket server
- **WHEN** an HTTP MCP server is configured with `socket_path = "/run/mcp.sock"` on Linux or macOS
- **THEN** the server connects over that socket and its tools are available

#### Scenario: Unsupported platform
- **WHEN** the same configuration is used on Windows
- **THEN** the server fails to connect with an error stating that Unix sockets are unsupported for that server

### Requirement: MCP server settings are validated wherever servers come from
The system SHALL validate MCP server settings both when loading configuration files and when connecting servers supplied by ACP clients. It SHALL reject, with an error naming the server: an empty socket path or a bare `@`; a socket path combined with OAuth; and OAuth combined with a static `Authorization` header.

#### Scenario: Invalid combination in a config file
- **WHEN** a configuration file declares an HTTP MCP server with both `socket_path` and OAuth
- **THEN** loading the configuration fails with an error naming the server

#### Scenario: Invalid client-supplied server
- **WHEN** an ACP client supplies an HTTP MCP server with an empty socket path
- **THEN** that server fails to connect with an error naming it, and the agent does not crash

### Requirement: New MCP settings have defaults
Every new MCP server setting SHALL have a default, so a configuration file that omits them loads: no OAuth, no socket path, tasks disabled, and caching enabled. Compatibility of the Rust configuration API is not required.

#### Scenario: Config without new keys
- **WHEN** a configuration with stdio and header-authenticated HTTP servers that uses none of the new keys is loaded
- **THEN** it parses and the servers connect without OAuth, without tasks, and with caching enabled
