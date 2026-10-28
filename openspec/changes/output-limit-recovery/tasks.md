# Tasks

> **API review (revised plan).** Tasks 1.1–1.4 and 2.2 were implemented with method pairs and a side channel that design Decisions 1 and 3 now reject. Do section 0 first, then the `r` rework tasks, then sections 6–7. Tasks still marked `[x]` describe behaviour that stays; the rework tasks replace how it is wired.

## 0. `ChatCall` API refactor (do first)

- [x] 0.1 Add `ChatCall<'a>` (`#[non_exhaustive]`, builder `new/tools/options`), `ChatCapabilities { streaming, options }` and the `ChatStream` alias to `querymt::chat`; reduce `ChatProvider` to `chat(&ChatCall)`, `chat_stream(&ChatCall)` and `capabilities()` with no request-forwarding defaults; remove `chat_with_tools`, `chat_stream_with_tools`, `*_and_options`, `supported_generation_options` and `supports_streaming`; verify `cargo check -p querymt`.
- [x] 0.2 Reduce `HTTPChatProvider` to `prepare_chat_request(&ChatCall, stream)`, `parse_chat(resp, context)`, `chat_stream_parser(context)`, `classify_chat_error` and `capabilities()`; remove `chat_request`, `chat_stream_request`, `prepare_chat_request_with_options`, `parse_chat_with_context` and `chat_stream_parser_with_context`; port `LLMProviderFromHTTP`; verify the adapter tests (options reach `prepare_chat_request`; requests without options are byte-identical).
- [x] 0.3 Port the wrappers (`tool_decorator`, `validated_llm` incl. retries, `chain`, `mesh_client`, `remote_chat_provider`) to forward the whole `ChatCall`; verify one wrapper test that an option survives the decorator chain.
- [x] 0.4 Port every HTTP provider under `crates/providers/` to the single `prepare_chat_request(call, stream)` (merge each provider's stream/non-stream builders) and to `capabilities()`; Anthropic keeps `with_generation_options`; port native providers (llama-cpp, mrs, izwi); verify `cargo check` per provider and the existing request-snapshot tests.
- [x] 0.5 Extism: reduce the macro exports to `prepare_chat_request`, `parse_chat_response`, the stream-parser exports and `capabilities`; drop the legacy `chat`/`chat_request`/`chat_stream_request` host paths; bump `PREPARED_CHAT_CONTRACT_VERSION`; verify a host/plugin round-trip with options and that an old-contract plugin fails with the rebuild error.
- [x] 0.6 Remote: make `build_chat_request` / `build_stream_request` take `ChatCall` (remove `*_with_options`), report the built provider's capabilities from `GetProviderContractInfo` instead of `ALL`, bump `MESH_PROTOCOL_VERSION`; verify the remote options test and the protocol-mismatch test.
- [x] 0.7 Port callers: agent (`session/provider.rs` collapses `submit_request` / `submit_request_with_options` into one taking `ChatCall`), the `MockLlmProvider` mock and its expectations, `querymt-py` (Python method names may stay as thin builders over `ChatCall`), examples and docs (`docs/core/chat.md`, `docs/plugins/development.md`); verify `cargo check` for affected packages and the Python package build.

## 1. Per-request generation options (querymt)

- [x] 1.1 Add `GenerationOptions` and `GenerationOptionSupport` to `querymt::chat`, plus `chat_with_tools_and_options`, `chat_stream_with_tools_and_options` and `supported_generation_options` on `ChatProvider` with defaults that ignore options; verify unit tests that existing implementers compile unchanged and the defaults delegate to the existing methods.
- [x] 1.2 Add an options-aware `prepare_chat_request` to `HTTPChatProvider` and forward options through `LLMProviderFromHTTP`, `tool_decorator`, `validated_llm` and `chain`; verify adapter tests that options reach `prepare_chat_request` and that calls without options produce identical requests.
- [x] 1.3 Add `#[serde(default)] options` to `ExtismChatRequest` (chat and stream) and pass it from the Extism host; verify a host/plugin round-trip test and that a request JSON without `options` still decodes.
- [x] 1.4 Add optional options to the `querymt-remote` chat request types and forward them in `remote_chat_provider`, `mesh_client` and `provider_backend`; verify a remote provider test that `max_tokens` reaches the backend provider and a compatibility test with the field absent.
- [x] 1.5 Make the Anthropic provider honour `max_tokens` and `reasoning_effort` per request (budget fitting and adaptive effort as in `anthropic-thinking-usage`) and report support; verify request-body tests for each override and for the plugin build.
- [x] 1.6 Document per-request generation options in the querymt docs (API and provider-author notes); verify the docs build.

## 2. Initial cap and ceiling

- [x] 2.1 Pass `ModelInfo` into `ModelDefaults::for_model`, compute `max_tokens = min(32_000, output_limit)` when unset and the output ceiling (explicit `max_tokens` is its own ceiling); verify `model_heuristics` tests for a 128k model, an unknown model, and an explicit 20k config.
- [x] 2.2 Carry the caps from `resolve_provider_config` to the session runtime (`SessionRuntime`/`ExecutionContext.output_caps`) and add `max_tokens` to `LlmRequestStart`; verify a provider-config test and an event serialization test, and regenerate typeshare types. *(Superseded by 2.2r.)*
- [x] 2.2r Replace `LastLlmRequest` / `SessionRuntime.last_llm_request` and `output_caps` with `ModelProfile { caps, capabilities, reasoning_effort, pricing }`, resolved with the provider, swapped on model switch and carried as `ConversationContext.profile`; remove `max_tokens` from `LlmRequestStart` (and its tests); record effective `gen_ai.request.*` values on the chat span; verify a provider-config test that the profile carries the caps, a model-switch test that the profile is replaced, and a span test for an escalated `max_tokens`.

## 3. Length-aware cut-off handling

- [x] 3.1 Return `Incomplete` from `ChatStreamAccumulator::finish` when the finish reason is `Length` and calls are open; keep `Failed(IncompleteToolCalls)` for other finish reasons; verify streaming unit tests for both cases.
- [x] 3.2 Change the skipped-call result text for length cut-offs to tell the model to split the write, and verify an execution test that a `Length` stream with an open call produces no `llm_retry_wait`, a skipped result and a `Stopped{ModelTokenLimit}`.
- [x] 3.3 Don't store thinking-only `Length` replies (no `add_message`, no `AssistantMessageStored`) while still updating stats; factor `apply_usage_to_stats` out of `transition_after_llm`; verify tests for thinking-only and mixed replies.

## 4. Request overrides and `on_stopped`

- [x] 4.1 Add `request_overrides` to `ConversationContext`, copy them into `PreparedModelRequest`, call the options-aware provider methods when they are supported, and clear the overrides after each request; verify execution tests that an override applies to exactly one request and is skipped (with a debug log) for unsupported providers. *(Wiring superseded by 4.1r.)*
- [x] 4.1r Build one `ChatCall` in `transition_call_llm`, filter options against `profile.capabilities.options`, and set `LlmResponse.applied` to the options actually sent; carry it as `last_applied` on the context that reaches `Stopped` (cleared at turn start); keep the existing one-request and unsupported-provider tests green.
- [x] 4.2 Add `MiddlewareDriver::on_stopped`, `CompositeDriver::run_stopped` and the phase-ending rules; verify driver tests for pass-through, recovery to `BeforeLlmCall` and ordering across two drivers.
- [x] 4.3 Call `run_stopped` from the `Stopped` branch in `agent/execution/mod.rs` with the per-turn `max_stop_recoveries` guard (default 5); verify execution tests that a recovering middleware suppresses `MiddlewareStopped` and that the guard publishes the stop.

## 5. OutputLimitRecoveryMiddleware

- [x] 5.1 Implement `middleware/output_limit.rs` with config (`enabled`, `escalate`, `max_continuations`, `instruction`, `lower_effort_on_thinking_only`), the factory (`type = "output_limit_recovery"`) and registration in `MiddlewarePresets::defaults()`; verify factory/preset tests.
- [x] 5.2 Implement escalation in `on_after_llm` (once per turn, below the ceiling, discarded reply's usage counted); verify middleware tests and an execution test that the escalated request uses the ceiling and history holds only the second reply.
- [x] 5.3 Implement continuation in `on_stopped` (fragment instruction, ceiling cap, effort lowering for thinking-only replies, removal after the next normal reply, cap at `max_continuations`); verify middleware tests for each branch and an execution test replaying this session's sequence (thinking-only cut-off → escalate → cut-off → continue → success).
- [x] 5.r Rework the middleware to read `context.profile` and `response.applied` / `context.last_applied` instead of `runtime.last_llm_request` (cap used = `applied.max_tokens.or(profile.caps.initial)`); verify the existing escalation, continuation and replay tests pass unchanged in behaviour.
- [x] 5.4 Add the `OutputLimitRecovery` event (turn, attempt, action, from/to cap, thinking-only, discarded message id), emitted on each step; verify event serialization and journal persistence tests and regenerate typeshare types.
- [x] 5.5 Document the middleware, its configuration and default instruction in the agent middleware docs, with an example TOML in `crates/agent/examples/confs`; verify the example config loads in a config test.

## 6. Run outcome and ACP reporting

- [x] 6.1 Add `RunCompletionDisposition::Stopped { stop_type, reason }`, carry it from `CycleOutcome::Stopped`, and emit `run_completed` with outcome `stopped` and the stop fields; verify session-actor tests and event serialization.
- [x] 6.2 Translate `MiddlewareStopped` and `OutputLimitRecovery` into live notices for notice-capable ACP clients (not replayed), keeping `PromptResponse` to the ACP stop reason only (no `_meta` stop fields); verify ACP tests for capable and non-capable clients (both get stop reason `max_tokens` for an output-limit stop, and the response has no `_meta.querymt` stop fields) and replay without notices.

## 7. Integration

- [x] 7.1 Run `cargo test` for `querymt`, `querymt-remote`, every provider crate touched in 0.4 (Anthropic native and Extism), `querymt-py` and `querymt-agent --all-features`, plus clippy and fmt; verify all green, and that the OCI plugin builds succeed with the bumped contract.
- [x] 7.2 Run a manual end-to-end check with `claude-sonnet-5-5`, using a prompt that asks for a very large single file write: the run escalates or continues instead of stopping, and the journal shows `output_limit_recovery` events; record the result in the change notes.

## Verification notes

### 7.1 Test run

- `cargo nextest run` was used throughout. Passing: `querymt` (224), `querymt-remote` (169), `querymt-py` and `querymt-agent-py` (with `PYO3_PYTHON` set to Python 3.14, since the default interpreter is 3.9 and the crate needs 3.10 or newer), Anthropic with the `native` feature (81), and the other provider crates (several have no tests).
- `querymt-agent --all-features`: all tests pass except `config::tests::example_profiles_keep_workspace_disabled_and_delegates_inheriting`, which failed only because of an unrelated uncommitted edit to `crates/agent/examples/confs/single_coder.toml` (it passes with that edit stashed). A few `execution_tests`, delegation and websocket tests timed out or failed in the full parallel run and pass when run alone (load flakes).
- `cargo clippy` is clean for `querymt`, `querymt-remote` and `querymt-agent` (all features, tests). `cargo fmt --check` is clean.
- Extism: `cargo b -p qmt-anthropic --release --target wasm32-wasip1` builds with the bumped contract. OCI plugin publishing is done by CI and was not run locally.

### 7.2 Manual end-to-end check

Run with `claude-sonnet-5-5` (Anthropic OAuth), the `from_config` example, a throwaway DB, and a config without `max_tokens` (initial cap 32,000, ceiling 128,000). Prompt: write `big.txt` with the numbers 1 to 15000 in a single `write_file` call.

| Journal seq | Event |
|---|---|
| 11 | `llm_request_end`: `finish_reason` Length after 31,832 output tokens, no tool call |
| 12 | `output_limit_recovery`: action `escalate`, attempt 1, 32,000 to 128,000, `thinking_only` false, cut-off message recorded as discarded |
| 13 | `llm_request_start`: same request retried; no assistant message stored between 11 and 13 |
| 16 | `llm_request_end`: ToolCalls after 44,077 output tokens (above the old cap, so the raised cap was sent) |
| 22 | `write_file` ran; `big.txt` has 15,000 lines |
| 26, 29 | Final reply `Stop`; `run_completed` outcome `completed` |

Message history afterwards holds 2 user and 2 assistant messages, none from the cut-off reply. The run used about 76k output tokens.

Not verified end to end (covered by unit tests only):

- The continuation path and the `stopped` run outcome. A cheap run with `max_tokens = 1024` did not exercise them: selecting a model through the dashboard rebuilt the session's LLM config without `[agent.parameters]`, so the 1024 cap never reached the request and nothing was cut off.
- The new stop and recovery notices in a live ACP client.

Unexplained: the journal of this run contains a `session_mode_changed` to `plan` (seq 14, 15) during the retried request, although `from_config` has no way to switch mode. See the follow-up below.

## Workflow follow-up

- Open a querymt-desktop change to render stop notices, the `stopped` run outcome and recovery events, and to drop discarded streamed messages.
- Open a small change to fix the `StopType` → ACP `StopReason` mapping: `ContentFilter` → `refusal` (currently `end_turn`), and reconsider `PriceLimit` → `max_tokens`.
- If a consumer needs fine-grained stop types over ACP (e.g. delegation failure messages in `delegation/core.rs`, which only see `MaxTokens`), propose a dedicated stop-reporting requirement naming that consumer.
- Open a small change to keep `[agent.parameters]` (e.g. `max_tokens`) when a session's model is selected. `apply_model_selection` (`session_actor.rs`) and `set_provider` (`handle/session_config.rs`) rebuild the LLM config from provider, model, system prompt and reasoning effort only, so the parameters are lost even when the same model is picked again; the dashboard does this at session start.
- Investigate the unexplained switch to `plan` mode during the 7.2 run (journal seq 14-15 in a `from_config` session with no other known client), and why `write_file` still ran afterwards.
