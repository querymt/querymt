# Proposal

## Why

A profile's `[agent.parameters]` (for example `max_tokens`, `n_ctx`, `enable_thinking`, KV-cache and sampling settings) are lost once a client selects a model for a session, even when it selects the profile's own model. On first use the session stores the full profile configuration, but every model selection then rebinds the session to a freshly built configuration holding only provider, model, system prompt and reasoning effort. Mesh clients select a model automatically, so on a remote llama.cpp node every such session ran with the provider's defaults (256 output tokens, thinking on) instead of the profile.

The root cause is the selection path discarding the stored configuration; provider construction is correct and needs no change.

## What Changes

- Model selection (`set_session_model`, `SetProvider`, `set_provider`) builds the stored configuration from the profile's configuration when the selected provider and model are exactly the profile's, and from an empty configuration otherwise. Identity, system prompt and reasoning effort are set on top as today. Because stored configurations deduplicate, re-selecting the profile's model lands on the profile's own configuration row.
- A model selection that names the provider, model and provider node the session is already bound to is a no-op: it does not rewrite the stored configuration, so an explicit configuration set through `set_llm_config` survives automatic re-selection.
- `set_llm_config` remains the explicit override path: its configuration is stored as given, without merging the profile.

## Capabilities

### New Capabilities
- None.

### Modified Capabilities
- `agent-configuration`: selecting the profile's model keeps the profile's parameters; re-selecting the current model keeps the session's configuration.

## Impact

- `crates/agent/src/agent/session_actor.rs` (`apply_model_selection`, `SetProvider`), `agent/handle/session_config.rs` (`set_provider`), plus a shared helper on `SessionProvider`.
- No change to provider construction (`session/provider_config.rs`) or the mesh provider host (`provider_host_backend.rs`).
- No configuration format or stored schema change. Existing sessions already bound to an incomplete configuration are not repaired; they regain the profile's parameters on their next selection of the profile's model from another model, or by a new session.

## Out of scope

- Scoping the mesh host's `host_default_params` to the profile's model (it currently applies to every requested model, which `model_path` alias resolution relies on). Track as a separate change if the leak to other models matters.
- Exposing per-session parameter overrides to ACP/UI clients. Different settings for the same model are expressed as separate profiles.
