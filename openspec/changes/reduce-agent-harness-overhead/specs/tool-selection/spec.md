# Spec Delta

## ADDED Requirements

### Requirement: Calls to unknown tools name the offered alternatives
When the model calls a tool name that matches no built-in, MCP or provider tool available to the agent, the call SHALL NOT run and its result SHALL be an error stating that the tool does not exist, followed by up to three offered tool names closest to the requested name. When no offered name is close, the result SHALL say so and name no tool. A tool that exists but is excluded by the session's allow or deny lists SHALL still be refused as not allowed.

#### Scenario: Prefixed name of an offered tool
- **WHEN** the model calls `mcp_todowrite` and the session is offered `todowrite`
- **THEN** the call does not run and its result states that `mcp_todowrite` does not exist and names `todowrite`

#### Scenario: Name with no close match
- **WHEN** the model calls `python` and no offered tool has a similar name
- **THEN** the result states that `python` does not exist and names no alternative

#### Scenario: Suggestions come only from offered tools
- **WHEN** the model calls `shel`, and `shell` exists but is denied for the session
- **THEN** the result states that `shel` does not exist and does not name `shell`

#### Scenario: Existing but denied tool
- **WHEN** a client denies `["shell"]` and the model calls `shell`
- **THEN** the call is refused as not allowed, with no list of alternatives

#### Scenario: Unknown tool in a batch
- **WHEN** a response contains a call to `grep_placeholder` and a call to `read_tool`
- **THEN** the `read_tool` call runs and the unknown call has exactly one error result
