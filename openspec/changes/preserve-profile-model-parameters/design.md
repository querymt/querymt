# Design

## Context

- The profile's `[agent.parameters]` reach `SessionProvider::initial_config` (`LLMParams`, typed fields plus `custom`).
- First use (`SessionActor::load_control_state_from_store`) stores the full `initial_config` via `create_or_get_llm_config`, which deduplicates identical configurations to one `llm_configs` row.
- Three selection paths rebuild the configuration from `LLMParams::new().provider().model()` plus system prompt (and effort): `SessionActor::apply_model_selection`, the `SetProvider` handler, and `LocalAgentHandle::set_provider`. None checks whether the selection changes anything, so re-selecting the current model rebinds the session to a stripped row.
- `SetLlmConfig` stores its configuration as given. `commit_reasoning_effort` re-reads the stored row and only changes the effort, so it preserves stored parameters.
- `resolve_provider_config` (`Runtime`) applies stored params, then `ModelDefaults` for missing keys. It is correct once the stored row is complete.
- Observed on a remote node: sessions bound to config 277 (system only) ran with `max_tokens=256` and thinking on, while config 281/282 (full profile) existed.

## Goals / Non-Goals

**Goals:**
- Selecting the profile's model stores the profile's configuration; selecting another model stores none of it.
- Re-selecting the current binding is idempotent, so automatic client selections cannot overwrite a session's configuration.
- One source of truth: the stored row describes what runs. Precedence follows from what is stored: explicit `set_llm_config` > profile (seeded at selection) > `ModelDefaults` (runtime, missing keys only).

**Non-Goals:**
- Runtime layering of profile parameters under stored parameters.
- Repairing or migrating existing `llm_configs` rows.
- Changing the mesh provider host's default parameters.
- Per-model parameter tables in profiles, or a UI/ACP override endpoint.

## Decisions

### D1. One selection constructor seeded from the profile
Add `SessionProvider::selection_params(provider, model) -> LLMParams`: a clone of `initial_config` when `provider`/`model` equal `initial_config.provider`/`.model` exactly, otherwise `LLMParams::new()`; then `.provider(provider).model(model)`. Callers set system prompt and reasoning effort on the result as they do today. Use it in `apply_model_selection`, `SetProvider` and `LocalAgentHandle::set_provider`.

*Why:* cloning the profile and overwriting identity, system and effort avoids maintaining a list of excluded keys. Exact provider+model matching keeps GGUF-specific settings such as `n_ctx`/`model_path` off other models. When system and effort equal the first-use values, the result is byte-identical to the first-use row and deduplicates to it. *Alternative:* runtime layering in `resolve_provider_config` — rejected: a second source of truth, makes profile keys impossible to unset per session, and makes old sessions drift with profile edits.

### D2. Idempotent selection
In `apply_model_selection`, after parsing `(provider, model)` with `parse_transport_model_id`, return the unchanged state (no commit, no events) when provider, model and normalised provider node equal `previous.effective_model`. Normalisation treats `None`, `"local"` and this node's own peer id as the same local node.

*Why:* mesh clients send a selection for the already-bound model; without this, D1 would re-seed from the profile and drop an explicit `set_llm_config` override. Switching to another model and back still re-seeds from the profile, which is the intended reset. *Alternative:* relying on dedupe alone — insufficient when the session's row differs from the profile (override, or legacy incomplete row).

### D3. `set_llm_config` stays the explicit override
No change. Its configuration is stored as given; omitting a profile key lets `ModelDefaults` apply. `SetProvider`/`set_provider` are selections, not overrides: they use D1, and since they bypass `apply_model_selection` (they go through `transition_model_with_config`), the D2 unchanged-binding check is applied on their path too.

## Risks / Trade-offs

- [Existing sessions bound to incomplete rows] → D2 keeps them as they are when re-selected; they recover by switching models or starting a new session. Accepted to avoid a migration; can be fixed by an explicit `set_session_model` away and back, or a one-off row rewrite if needed.
- [Profile edited after sessions were stored] → stored rows keep the values from selection time. Intended: the stored row is authoritative.
- [Provider node normalisation misses a case] → a redundant selection commits a new binding; with D1 the stored params are still the profile's, so the failure mode is the old override loss, not the original bug. Covered by tests for `None`, `"local"` and own peer id.
- [Mesh host still applies profile params to all models] → unchanged behaviour, out of scope.

## Migration Plan

No data migration. Rollback restores the old behaviour; rows written under D1 remain valid configurations.
