# chat-generation-options Specification

## Purpose

Lets callers override generation settings such as the output-token cap and reasoning effort for a single chat request, without rebuilding the provider, across local, plugin and remote providers.

## Requirements

### Requirement: Chat requests accept per-request generation options
Chat operations, streaming and non-streaming, SHALL take a single request value carrying the messages, optional tools and optional per-request generation options (at least `max_tokens` and `reasoning_effort`). A set option SHALL override the provider's configured value for that request only. Unset options SHALL leave the configured values in effect. Adding a request parameter SHALL NOT require adding a provider method.

#### Scenario: Override applies to one request
- **WHEN** a provider configured with `max_tokens = 32000` receives a request with option `max_tokens = 128000`, followed by a request without options
- **THEN** the first request is sent with 128,000 and the second with 32,000

#### Scenario: Requests without options are unchanged
- **WHEN** a caller sends a request with no options set
- **THEN** the request body is identical to the body produced before this change

#### Scenario: Wrappers forward the whole request
- **WHEN** a request with option `max_tokens = 64000` passes through a provider wrapper (tool decorator, validation, mesh)
- **THEN** the wrapped provider receives the option unchanged

### Requirement: Options cross every provider transport
Per-request options SHALL be carried unchanged through the HTTP adapter, the plugin interface and the remote provider transport. Plugins or remote peers built against the previous contract SHALL be rejected by the contract/protocol version check with an actionable error, not silently ignore options.

#### Scenario: Plugin provider
- **WHEN** a request with option `reasoning_effort = low` is sent to the Anthropic plugin build
- **THEN** the plugin receives the option and the outgoing request uses low effort

#### Scenario: Remote provider
- **WHEN** a request with option `max_tokens = 64000` is sent through a remote provider on another node
- **THEN** the remote node's provider sends the request with 64,000

#### Scenario: Outdated plugin
- **WHEN** the host loads a plugin built against the previous prepared-chat contract
- **THEN** the chat request fails with an error telling the user to rebuild the plugin

### Requirement: Providers report which options they honour
Each provider SHALL report its chat capabilities (streaming and which per-request options it honours) through one capabilities query, and callers SHALL be able to query this before sending. A remote host SHALL report the capabilities of the provider it builds, not assume all options. Options a provider does not honour SHALL be ignored without failing the request.

#### Scenario: Unsupported option
- **WHEN** a provider that does not honour `max_tokens` overrides receives one
- **THEN** the request succeeds with the configured value and the provider reports `max_tokens` as unsupported
