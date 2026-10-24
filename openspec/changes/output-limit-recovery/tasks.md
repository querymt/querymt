# Tasks

## 1. Per-request generation options (querymt)

- [x] 1.1 Add `GenerationOptions` and `GenerationOptionSupport` to `querymt::chat`, plus `chat_with_tools_and_options`, `chat_stream_with_tools_and_options` and `supported_generation_options` on `ChatProvider` with defaults that ignore options; verify unit tests that existing implementers compile unchanged and the defaults delegate to the existing methods.
- [x] 1.2 Add an options-aware `prepare_chat_request` to `HTTPChatProvider` and forward options through `LLMProviderFromHTTP`, `tool_decorator`, `validated_llm` and `chain`; verify adapter tests that options reach `prepare_chat_request` and that calls without options produce identical requests.
- [x] 1.3 Add `#[serde(default)] options` to `ExtismChatRequest` (chat and stream) and pass it from the Extism host; verify a host/plugin round-trip test and that a request JSON without `options` still decodes.
- [x] 1.4 Add optional options to the `querymt-remote` chat request types and forward them in `remote_chat_provider`, `mesh_client` and `provider_backend`; verify a remote provider test that `max_tokens` reaches the backend provider and a compatibility test with the field absent.
- [x] 1.5 Make the Anthropic provider honour `max_tokens` and `reasoning_effort` per request (budget fitting and adaptive effort as in `anthropic-thinking-usage`) and report support; verify request-body tests for each override and for the plugin build.
- [x] 1.6 Document per-request generation options in the querymt docs (API and provider-author notes); verify the docs build.

## 2. Initial cap and ceiling

- [x] 2.1 Pass `ModelInfo` into `ModelDefaults::for_model`, compute `max_tokens = min(32_000, output_limit)` when unset and the output ceiling (explicit `max_tokens` is its own ceiling); verify `model_heuristics` tests for a 128k model, an unknown model, and an explicit 20k config.
- [x] 2.2 Carry the caps from `resolve_provider_config` to the session runtime (`SessionRuntime`/`ExecutionContext.output_caps`) and add `max_tokens` to `LlmRequestStart`; verify a provider-config test and an event serialization test, and regenerate typeshare types.

## 3. Length-aware cut-off handling

- [x] 3.1 Return `Incomplete` from `ChatStreamAccumulator::finish` when the finish reason is `Length` and calls are open; keep `Failed(IncompleteToolCalls)` for other finish reasons; verify streaming unit tests for both cases.
- [x] 3.2 Change the skipped-call result text for length cut-offs to tell the model to split the write, and verify an execution test that a `Length` stream with an open call produces no `llm_retry_wait`, a skipped result and a `Stopped{ModelTokenLimit}`.
- [x] 3.3 Don't store thinking-only `Length` replies (no `add_message`, no `AssistantMessageStored`) while still updating stats; factor `apply_usage_to_stats` out of `transition_after_llm`; verify tests for thinking-only and mixed replies.

## 4. Request overrides and `on_stopped`

- [x] 4.1 Add `request_overrides` to `ConversationContext`, copy them into `PreparedModelRequest`, call the options-aware provider methods when they are supported, and clear the overrides after each request; verify execution tests that an override applies to exactly one request and is skipped (with a debug log) for unsupported providers.
- [x] 4.2 Add `MiddlewareDriver::on_stopped`, `CompositeDriver::run_stopped` and the phase-ending rules; verify driver tests for pass-through, recovery to `BeforeLlmCall` and ordering across two drivers.
- [x] 4.3 Call `run_stopped` from the `Stopped` branch in `agent/execution/mod.rs` with the per-turn `max_stop_recoveries` guard (default 5); verify execution tests that a recovering middleware suppresses `MiddlewareStopped` and that the guard publishes the stop.

## 5. OutputLimitRecoveryMiddleware

- [x] 5.1 Implement `middleware/output_limit.rs` with config (`enabled`, `escalate`, `max_continuations`, `instruction`, `lower_effort_on_thinking_only`), the factory (`type = "output_limit_recovery"`) and registration in `MiddlewarePresets::defaults()`; verify factory/preset tests.
- [x] 5.2 Implement escalation in `on_after_llm` (once per turn, below the ceiling, discarded reply's usage counted); verify middleware tests and an execution test that the escalated request uses the ceiling and history holds only the second reply.
- [x] 5.3 Implement continuation in `on_stopped` (fragment instruction, ceiling cap, effort lowering for thinking-only replies, removal after the next normal reply, cap at `max_continuations`); verify middleware tests for each branch and an execution test replaying this session's sequence (thinking-only cut-off → escalate → cut-off → continue → success).
- [x] 5.4 Add the `OutputLimitRecovery` event (turn, attempt, action, from/to cap, thinking-only, discarded message id), emitted on each step; verify event serialization and journal persistence tests and regenerate typeshare types.
- [x] 5.5 Document the middleware, its configuration and default instruction in the agent middleware docs, with an example TOML in `crates/agent/examples/confs`; verify the example config loads in a config test.

## 6. Run outcome and ACP reporting

- [ ] 6.1 Add `RunCompletionDisposition::Stopped { stop_type, reason }`, carry it from `CycleOutcome::Stopped`, and emit `run_completed` with outcome `stopped` and the stop fields; verify session-actor tests and event serialization.
- [ ] 6.2 Translate `MiddlewareStopped` and `OutputLimitRecovery` into live notices for notice-capable ACP clients (not replayed), keeping `PromptResponse` to the ACP stop reason only (no `_meta` stop fields); verify ACP tests for capable and non-capable clients (both get stop reason `max_tokens` for an output-limit stop, and the response has no `_meta.querymt` stop fields) and replay without notices.

## 7. Integration

- [ ] 7.1 Run `cargo test` for `querymt`, `querymt-remote`, `qmt-anthropic` (native and Extism) and `querymt-agent --all-features`, plus clippy and fmt; verify all green.
- [ ] 7.2 Run a manual end-to-end check with `claude-sonnet-5-5`, using a prompt that asks for a very large single file write: the run escalates or continues instead of stopping, and the journal shows `output_limit_recovery` events; record the result in the change notes.

## Workflow follow-up

- Open a querymt-desktop change to render stop notices, the `stopped` run outcome and recovery events, and to drop discarded streamed messages.
- Open a small change to fix the `StopType` → ACP `StopReason` mapping: `ContentFilter` → `refusal` (currently `end_turn`), and reconsider `PriceLimit` → `max_tokens`.
- If a consumer needs fine-grained stop types over ACP (e.g. delegation failure messages in `delegation/core.rs`, which only see `MaxTokens`), propose a dedicated stop-reporting requirement naming that consumer.
