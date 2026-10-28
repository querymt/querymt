# Design

## Context

Current flow (see proposal.md, Why):

```
BeforeLlmCall --on_step_start--> CallLlm --> AfterLlm --on_after_llm--> transition_after_llm
                                                                         | persist + stats
                                       Length --> Stopped{ModelTokenLimit} --> MiddlewareStopped, run ends (no hook)
```

- `transition_after_llm` (`agent/execution/transitions.rs:1295`) always stores the assistant message and updates stats, then maps `Length` to `Stopped` (lines 1512-1527 and 1560-1564).
- `Stopped` is handled in `agent/execution/mod.rs:~770-817`, which emits `MiddlewareStopped` and returns `CycleOutcome::Stopped`. No middleware runs.
- `ChatStreamAccumulator::finish` (`querymt/src/chat/streaming.rs:360`) returns `Failed(IncompleteToolCalls)` when calls are still open after `Done`, whatever the finish reason. `stream_accumulation_error` (`transitions.rs:275`) wraps that as `UnknownTransient`, so the retry policy retries it.
- Provider settings are fixed when the provider is built (`resolve_provider_config`, `session/provider_config.rs:89-113`), and the session caches the provider (`session/provider.rs:801`). `ChatProvider` (`querymt/src/chat/mod.rs:643`) has no per-request settings.
- `ModelDefaults::for_model` has no model metadata. `get_model_info` (`model_info.rs:99`) reads the models.dev cache. Both already include `limit.output`.
- `ConversationContext` fragments are temporary and are added at request time (`middleware/state.rs:181-251`).
- `RunCompletionDisposition` has only Completed, Failed and Cancelled (`agent/session_actor.rs:452-456`, `583-587`).

## Goals / Non-Goals

**Goals:** the requirements in `specs/` for `chat-generation-options`, `output-limit-recovery` and `acp-session-updates`.

**Non-Goals:**
- Rendering in querymt-desktop (separate change in that repo).
- Recovering from other stop types. `on_stopped` makes that possible later, e.g. compaction on `ContextThreshold`.
- Continuing a cut-off *text* reply by prefilling the assistant turn. We continue with an instruction instead.
- Per-request options in providers other than Anthropic.
- Changing the `StopType` → ACP `StopReason` mapping (`events.rs:39-50`). Two entries are wrong or questionable and are tracked as follow-ups: `ContentFilter → end_turn` should be `refusal`, and `PriceLimit → max_tokens` reports a cost budget as a token limit.
- Exposing the fine-grained QueryMT stop type to generic ACP clients. The mapping collapses eight stop types into three ACP values. If a consumer needs the difference, for example delegation reporting "output cap" vs "price limit", that is a separate stop-reporting requirement with a named user.

## Decisions

### 1. `GenerationOptions` in querymt (D3)

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GenerationOptions {
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<ReasoningEffort>,
}
```

**Revised (API review):** options go on a single request value, not on new method pairs. The first implementation added `chat_with_tools_and_options` / `chat_stream_with_tools_and_options` / `prepare_chat_request_with_options` next to the existing `chat` / `chat_with_tools` / `*_with_context` pairs. Each new request dimension doubled the trait surface. The defaults also silently dropped options for wrappers that only overrode the older method. That code is replaced as follows:

```rust
#[non_exhaustive]
pub struct ChatCall<'a> {              // not `ChatRequest`: clashes with api::types::ChatRequest
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [Tool]>,
    pub options: GenerationOptions,
}
// ChatCall::new(&msgs).tools(&tools).options(opts)

trait ChatProvider {
    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError>;
    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError>;
    fn capabilities(&self) -> ChatCapabilities; // { streaming: bool, options: GenerationOptionSupport }
}

trait HTTPChatProvider {
    fn prepare_chat_request(&self, call: &ChatCall<'_>, stream: bool) -> Result<PreparedChatRequest, LLMError>;
    fn parse_chat(&self, resp: Response<Vec<u8>>, context: Value) -> Result<ChatOutput, LLMError>;
    fn chat_stream_parser(&self, context: Value) -> Result<Box<dyn ChatStreamParser>, LLMError>;
    fn classify_chat_error(&self, resp: &Response<Vec<u8>>) -> LLMError;
    fn capabilities(&self) -> ChatCapabilities;
}
```

- **Removed:** `chat_with_tools`, `chat_stream_with_tools`, both `*_and_options` methods, `supported_generation_options`, `supports_streaming` (all on `ChatProvider`). Also `chat_request`, `chat_stream_request`, `prepare_chat_request_with_options`, `parse_chat_with_context` and `chat_stream_parser_with_context` (on `HTTPChatProvider`). No compatibility shims (AGENTS.md); nothing here is released.
- There are no request-forwarding defaults. Wrappers (`tool_decorator`, `validated_llm`, `chain`, `adapters`, `mesh_client`, `remote_chat_provider`, the Extism host) forward the whole `ChatCall`, so a new field cannot be dropped silently.
- A future request parameter (temperature, stop sequences, …) is a new field on `GenerationOptions` or `ChatCall`, never a new method.
- HTTP providers build streaming and non-streaming requests in one function keyed by `stream` (most already do `cfg.stream = Some(true); chat_request(..)`).
- `ChatStream` is a type alias for the boxed chunk stream.
- Extism: `ExtismChatRequest` keeps `options: Option<GenerationOptions>`. The plugin exports collapse to `prepare_chat_request`, `parse_chat_response`, the stream-parser exports and `capabilities`. `PREPARED_CHAT_CONTRACT_VERSION` is bumped and every plugin is rebuilt; the host does not support the old exports.
- Remote: `ProviderChatRequest` / `ProviderStreamRequest` keep `options`. `build_chat_request` / `build_stream_request` take a `ChatCall` (the `*_with_options` builders are removed). `MESH_PROTOCOL_VERSION` is bumped.
- Remote capability reporting: the host answers `GetProviderContractInfo` with the options the *built provider* honours, not a blanket `ALL`. The handshake also carries the host-resolved initial cap, output ceiling and reasoning effort. Session provider construction negotiates these facts before snapshotting `ModelProfile`, respecting host defaults and explicit caps. Mesh protocol version 4 requires peers to ship this profile together.
- **Alternative rejected:** building a one-off provider per escalated request. It is cheap, but it bypasses provider caching, OAuth resolver reuse and plugin instance locking, and it doesn't extend to effort changes.
- **Alternative rejected:** keeping method pairs with default delegation. This was the first implementation; it grows without bound and fails silently.

### 2. Initial cap and ceiling (D1)

- `ModelDefaults::for_model(provider, model, model_info: Option<&ModelInfo>)`. If the config has no `max_tokens`, it sets `max_tokens = min(32_000, output_limit)` (32k when unknown, Anthropic only, as today).
- It also returns `output_ceiling = output_limit.unwrap_or(max_tokens)`.
- If the user set `max_tokens`, `output_ceiling = max_tokens`, so escalation never goes past an explicit choice.
- **Revised (API review):** the caps are part of a per-session `ModelProfile` (see Decision 3), built once when the provider is resolved and replaced on model switch. They are not a separate `exec_ctx.output_caps` value.
- Non-Anthropic providers keep their current defaults. The ceiling is still computed so the middleware can escalate wherever per-request options are supported.

### 3. Request overrides travel on `ConversationContext`

- Add `request_overrides: GenerationOptions` (an Arc'd field, default empty) with builder-style `with_request_overrides`.
- `transition_before_llm_call` copies it into `PreparedModelRequest.options`.
- `transition_call_llm` drops options the provider does not honour (per `profile.capabilities.options`), logging them at debug level, and sends the rest.
- Overrides last for one request: `transition_after_llm` builds the new context without them.
- `transition_call_llm` builds one `ChatCall` (Decision 1) from `PreparedModelRequest` and calls `chat` / `chat_stream`. It never picks between method variants.

**Revised (API review): no side channel, no event field.** The first implementation added `LlmRequestStart.max_tokens` and `SessionRuntime.last_llm_request: Mutex<LastLlmRequest>`. The execution loop wrote that struct before each request and the middleware read it back. Both are removed:

- `LlmRequestStart` stays `{ message_count }`. Every request parameter added there would become a persisted, typeshare-exported wire commitment, and nothing outside tests reads it. Clients see caps through `OutputLimitRecovery { from_max_tokens, to_max_tokens }`.
- Effective request parameters go to telemetry. The chat span records `gen_ai.request.max_tokens` (and the other `gen_ai.request.*` attributes) from the *effective* values (configured values with overrides applied), not only the configured ones (`GenerationSettings::record` today).
- **Per-session facts** go into one `ModelProfile { caps: Option<OutputCaps>, capabilities: ChatCapabilities, reasoning_effort: Option<ReasoningEffort>, pricing: Option<ModelPricing> }`. It is resolved with the provider, swapped atomically on model switch (the same path as `provider` / `model`) and carried on `ConversationContext` as `profile: Arc<ModelProfile>`.
- **Per-request fact:** `LlmResponse` gains `applied: GenerationOptions`, the overrides the request was actually sent with (after dropping unsupported ones). A response describes its own request, so nothing has to be reconstructed from "the last request".
- The middleware derives the cap that was used as `response.applied.max_tokens.or(profile.caps.map(|c| c.initial))`. `on_stopped` reads the cap from the `Stopped` state's context. That context carries the response's `applied` value forward as `last_applied: GenerationOptions`, and it is cleared at turn start.
- **Alternative rejected:** a mutex-guarded "last request" struct on `SessionRuntime`. It works around the state machine, mixes session facts with request facts, and breaks if anything else issues a request in between (compaction, retries).

### 4. Cut-off classification

- In `ChatStreamAccumulator::finish`: if `finish_reason == Some(Length)` and calls are still open, return `Incomplete { output, detail: "output token limit" }`, with the unfinished calls kept in the output but not executable.
- Structured-mode providers already go through `Incomplete`.
- This uses the existing `Incomplete` path in `transition_call_llm` (line 1102), which doesn't retry, and the `output_status == Incomplete` branch of `transition_after_llm`. That branch adds skipped results for calls, with the text changed to "truncated by output token limit; split the write into smaller calls".
- The existing test that expects `IncompleteToolCalls` for `Done{ToolCalls}` stays. Only `Length` changes.

### 5. `on_stopped` phase (decision 1)

- `MiddlewareDriver::on_stopped(state, runtime)`, default pass-through, and `CompositeDriver::run_stopped`.
- In `run_phase`, during `Stopped`, the phase ends on `BeforeLlmCall` (recovered) or `Cancelled`. A `Stopped` result moves on to the next driver, so later middleware can also act.
- `mod.rs` `Stopped` branch:
  1. If `stop_recoveries_this_turn < max_stop_recoveries` (default 5) and `context.is_some()`, call `run_stopped`.
  2. On `BeforeLlmCall`, increment the counter, emit nothing, and `continue`.
  3. Otherwise emit `MiddlewareStopped` as today.
- The counter lives on `ExecutionContext` and resets at turn start. It guards against any middleware looping.
- Drivers order: `OutputLimitRecoveryMiddleware` is registered before `LimitsMiddleware`/`ContextMiddleware` in presets. It only reacts to `ModelTokenLimit`, so order matters only for log clarity.

### 6. `OutputLimitRecoveryMiddleware`

```
state per turn: escalated: bool, escalated_cap: Option<u32>, continuations: u8, fragment_active: bool

on_after_llm(AfterLlm{response, context}):
  cut = response.finish_reason == Length
  if !cut && fragment_active -> remove fragment (keep the per-turn continuation count)
  if cut && !escalated && last_cap < ceiling:
      escalated = true; escalated_cap = context-clamped target; emit Recovery{escalate}
      record_discarded_usage(response.usage)          // via runtime hook, see 7
      return BeforeLlmCall(context.with_request_overrides(max_tokens=ceiling))
  pass

on_stopped(Stopped{ModelTokenLimit, context}):
  if continuations < max_continuations:
      continuations += 1; emit Recovery{continue}
      ctx = context.upsert_fragment("output_limit_recovery", instruction, High)
      overrides.max_tokens = escalated_cap (if escalated)
      if thinking_only: overrides.reasoning_effort = lower(current effort, floor Low; Auto->Medium)
      return BeforeLlmCall(ctx.with_request_overrides(overrides))
  pass
```

- Continuation guidance follows a retained assistant reply rather than creating assistant prefill. For a latest tool-result message, fragments are appended to its input parts to preserve tool-use/result adjacency. Latest durable user feedback otherwise stays after fragments.
- Pending request overrides survive queued steering. After history, tools and context hooks are prepared, output overrides are clamped again against the available context window.
- Because `on_after_llm` returns `BeforeLlmCall`, `transition_after_llm` doesn't run: the escalated attempt's reply is never stored. The streamed partial message is closed by the recovery event, which carries `discarded_message_id` so clients drop it.
- Config (TOML, all optional): `enabled = true`, `escalate = true`, `max_continuations = 3`, `instruction = "<default text>"`, `lower_effort_on_thinking_only = true`.
- Default instruction: "Your previous response was cut off by the output-token limit ({cap} tokens). Continue directly from where you stopped without repeating or summarising earlier output. Split large file writes or edits into several smaller tool calls, and keep reasoning brief."
- `reset()` clears the per-turn state. The middleware reads the ceiling from `runtime` (`SessionRuntime` exposes the output caps).

### 7. Thinking-only replies and accounting

- In `transition_after_llm`, a `Length` reply with no text, no tool calls and only reasoning items is not stored with `add_message` and emits no `AssistantMessageStored`. Stats are still updated, and it goes to `Stopped{ModelTokenLimit}`.
- For discarded escalation attempts, a shared helper `apply_usage_to_stats(stats, usage, pricing)`, factored out of `transition_after_llm`, is applied before returning `BeforeLlmCall`. `ConversationContext.stats` therefore includes the attempt, and the session's persisted usage, from the `LlmRequestEnd` event, already counts it.

### 8. Run outcome and ACP

- `RunCompletionDisposition::Stopped { stop_type, reason }`. `run_completed.outcome = "stopped"` gains optional `stop_type` and `reason` fields, which are additive.
- `CycleOutcome::Stopped` carries the type through to `session_actor`.
- `acp/shared.rs` translates `MiddlewareStopped` into `SessionUpdate::Notice` (Warning) when notices are negotiated. Notices are live-only and are not replayed.
- `PromptResponse` carries only the ACP stop reason from `StopReason::from(stop_type)` (`ModelTokenLimit → max_tokens`, unchanged). **Alternative rejected:** `_meta.querymt.{stopType,reason}`. For this change it would duplicate `stopReason: "max_tokens"`. Notice-capable clients already get the cause, querymt-desktop reads `MiddlewareStopped` and `run_completed` from the native event stream, and generic ACP clients don't read `_meta.querymt`. A free-text `reason` would also invite parsing.
- `OutputLimitRecovery` becomes an info notice ("Output limit hit; retrying at 128k (1/3)").

## Risks / Trade-offs

- **[Escalation doubles the cost of a cut-off step]** → One escalation per turn at most. The ceiling equals the model's real limit, and usage is reported accurately.
- **[A large `max_tokens` on old models must fit input + output within the context window]** → Escalation cap = `min(ceiling, context_limit - estimated_input_tokens)` when the context limit is known.
- **[Remote peer on an older version ignores options]** → Escalation then has no effect, and the middleware sees a second `Length` at the same cap and moves on to continuation. This is acceptable. With `MESH_PROTOCOL_VERSION` bumped, an older peer fails the contract handshake with an actionable error instead of silently ignoring options. Capabilities reported by the remote host (from the built provider) let the middleware skip escalation.
- **[The instruction fragment could persist if the turn ends abnormally]** → Fragments are per-context and `reset()` clears state at the next turn.
- **[Breaking `ChatProvider` / `HTTPChatProvider` / Extism ABI / mesh protocol]** → Intentional (API review). Nothing is released; all in-tree implementers, examples, Python bindings and docs move in the same change, and every OCI plugin is rebuilt. Out-of-tree plugins fail fast on the contract version check.
- **[Changed `run_completed.outcome` value]** → Additive. Clients that don't know `"stopped"` treat it as unknown. querymt-desktop will be updated.

## Migration Plan

- No storage migration: new event variant and fields use serde defaults. `LlmRequestStart` keeps its pre-change shape.
- Order of work: land the `ChatCall` API refactor first (tasks section 0), then rework the already-implemented recovery code onto it (tasks 2.2r, 4.1r, 5.xr).
- Ship querymt, providers, plugins and agent together. Mesh peers and plugins on older versions are rejected by the contract/protocol version checks.
- Rollback: set `[[middleware]] type = "output_limit_recovery", enabled = false`, or remove it from the presets.
