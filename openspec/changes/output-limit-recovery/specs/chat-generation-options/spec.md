# Spec Delta

## Purpose

Lets callers override generation settings such as the output-token cap and reasoning effort for a single chat request, without rebuilding the provider, across local, plugin and remote providers.

## ADDED Requirements

### Requirement: Chat requests accept per-request generation options
Chat operations, streaming and non-streaming, SHALL accept optional per-request generation options with at least `max_tokens` and `reasoning_effort`. A set option SHALL override the provider's configured value for that request only. Unset options SHALL leave the configured values in effect. Existing calls without options SHALL behave as before.

#### Scenario: Override applies to one request
- **WHEN** a provider configured with `max_tokens = 32000` receives a request with option `max_tokens = 128000`, followed by a request without options
- **THEN** the first request is sent with 128,000 and the second with 32,000

#### Scenario: Calls without options are unchanged
- **WHEN** a caller uses the existing chat methods without options
- **THEN** the request body is identical to the body produced before this change

### Requirement: Options cross every provider transport
Per-request options SHALL be carried unchanged through the HTTP adapter, the plugin interface and the remote provider transport. Older plugins or remote peers that do not understand options SHALL keep working, with the options ignored.

#### Scenario: Plugin provider
- **WHEN** a request with option `reasoning_effort = low` is sent to the Anthropic plugin build
- **THEN** the plugin receives the option and the outgoing request uses low effort

#### Scenario: Remote provider
- **WHEN** a request with option `max_tokens = 64000` is sent through a remote provider on another node
- **THEN** the remote node's provider sends the request with 64,000

### Requirement: Providers report which options they honour
Each provider SHALL report which per-request options it honours, and callers SHALL be able to query this before sending. Options a provider does not honour SHALL be ignored without failing the request.

#### Scenario: Unsupported option
- **WHEN** a provider that does not honour `max_tokens` overrides receives one
- **THEN** the request succeeds with the configured value and the provider reports `max_tokens` as unsupported
