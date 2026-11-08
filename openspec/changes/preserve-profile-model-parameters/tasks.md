# Tasks

## 1. Selection constructor

- [x] 1.1 Add `SessionProvider::selection_params(provider, model) -> LLMParams` (clone of `initial_config` on exact provider+model match, else `LLMParams::new()`, then identity set); unit tests in `session/provider.rs` for match, other model, other provider, and that `custom` (e.g. `n_ctx`) is carried only on match
- [x] 1.2 Use it in `SessionActor::apply_model_selection` (reached by the `SetProvider` handler and `LocalAgentHandle::set_provider`), keeping system prompt and reasoning effort handling; session-actor tests: selecting the profile's model stores the profile's `max_tokens` and reuses the first-use `llm_config_id`; selecting another model stores none; switching away and back restores them

## 2. Idempotent selection

- [x] 2.1 In `apply_model_selection` (shared by `set_session_model`, `SetProvider` and `set_provider`), return without committing when provider, model and normalised provider node equal the current binding; tests for node given as `None`, `"local"` and own peer id, asserting no `ProviderChanged` event
- [x] 2.2 Test that a `set_llm_config` override (`max_tokens = 4096`) survives a subsequent `set_session_model` for the same model

## 3. End to end

- [x] 3.1 Execution test: a session receiving `set_session_model` for the profile's model builds its provider with the profile's `max_tokens` and `enable_thinking` (mock provider factory captures the config)

## 4. Validation

- [x] 4.1 `cargo nextest run -p querymt-agent`, `cargo clippy -p querymt-agent --all-targets`, `cargo fmt --check`; `openspec validate preserve-profile-model-parameters --strict`
  - `cargo nextest run -p querymt-agent --features remote --no-fail-fast` (nextest 0.9.136): 4155 passed, 12 skipped. One unrelated test, `remote_connect::tests::connect_budget_defaults_are_bounded_and_reason_specific`, fails only because my shell exports `QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS=20000` (the test guards just that var, not the reconnect one); it passes with the var unset.
  - Default-feature focused run of the new tests: 8 passed. Remote-only own-peer-id test passes under `--features remote`.
  - `cargo clippy -p querymt-agent --all-targets --features remote`: no diagnostics in the touched files (pre-existing warnings elsewhere). `cargo fmt --check`: clean. `openspec validate ... --strict`: valid.
  - Not run: dashboard/oauth/zbus/systemd features.
- [ ] 4.2 Manual: on the remote node, start a new session from a mesh client and confirm the log shows `max_tokens=16384` and `starts_in_thinking=false`

## Workflow follow-up

- Archive the change after review.
- Separate change, if needed: scope mesh host `host_default_params` to the profile's model (with `model_path` alias handling).
