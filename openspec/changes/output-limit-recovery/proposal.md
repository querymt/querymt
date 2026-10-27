# Proposal

## Why

When a model reply hits its output-token cap (`finish_reason = Length`), the agent ends the run straight away. It calls this `Stopped{ModelTokenLimit}`, reports the run as `completed`, and clients get no notice. Session `01a16d18-6bf3-7219-849e-967cc8b81124` ended this way after one reply used all 32,000 output tokens on thinking. Three things made it worse:

- The cap is a fixed 32k for every Anthropic model (`model_heuristics.rs`), although the model allowed 128k.
- No middleware can react, because `Stopped` never goes through the middleware pipeline.
- Two earlier replies that were probably also cut off mid-tool-call were retried as transient stream errors, with 50–55 s waits and the partial output thrown away.

## What Changes

- **Per-request generation options in querymt, via one request value.** Chat calls take a single `ChatCall { messages, tools, options }`, and its optional settings (`max_tokens`, `reasoning_effort`) override the provider's configured values for that one request. **BREAKING (API):** `ChatProvider` collapses to `chat` / `chat_stream` / `capabilities`, and `HTTPChatProvider` to `prepare_chat_request` / `parse_chat` / `chat_stream_parser` / `classify_chat_error` / `capabilities`. The `*_with_tools`, `*_and_options`, `*_with_options` and `*_with_context` variants are removed. Options are carried by the HTTP adapter, the Extism plugin interface (contract version bumped) and the remote mesh (protocol version bumped). Providers report what they honour through `capabilities()`. The Anthropic provider is the first to honour options.
- **Starting `max_tokens` from model metadata.** When no `max_tokens` is configured, the cap is `min(32,000, model output limit)`, and the model output limit is recorded as the escalation ceiling. This replaces the fixed Anthropic 32k heuristic. An explicit `max_tokens` is always respected and turns escalation off.
- **Cut-offs become length outcomes.** A stream that ends with `Length` and unfinished tool calls is treated as an incomplete, length-limited reply, not a transient error. It is no longer retried, and its partial output is kept.
- **New `on_stopped` middleware phase.** It runs before a stop is published. Middleware may turn the stop into `BeforeLlmCall` to keep going, with a per-turn loop guard.
- **New `OutputLimitRecoveryMiddleware`** (`type = "output_limit_recovery"`, on by default):
  - In `on_after_llm`, it retries a cut-off reply once at the escalation ceiling and discards the cut-off reply.
  - In `on_stopped`, it continues up to N times (default 3) with a configurable fixed instruction: resume without recap, split large writes into smaller tool calls. For thinking-only cut-offs it also lowers reasoning effort for that request.
  - The instruction is removed after the next reply that isn't cut off.
- **Accounting.** Discarded attempts still count towards usage and cost. Thinking-only cut-offs are not stored in history.
- **Request facts travel with state, not side channels.** A per-session `ModelProfile` (caps, capabilities, effort, pricing) rides on `ConversationContext`, and `LlmResponse.applied` records the options a request was sent with. `LlmRequestStart` gains no fields, and `SessionRuntime` gains no last-request state. Effective `gen_ai.request.*` values go to the chat span.
- **Visibility.**
  - Recovery steps produce an `output_limit_recovery` event.
  - Runs that end on a stop report outcome `stopped`, with the stop type, instead of `completed`. **BREAKING (event payload):** `run_completed.outcome` can now be `"stopped"`.
  - ACP clients get the matching ACP stop reason (`max_tokens` for an output-limit stop, as today) and, if they support notices, a live warning `notice`. No QueryMT stop metadata is added to `PromptResponse`: ACP's `stopReason` already carries the protocol-level signal.

## Capabilities

### New Capabilities
- `chat-generation-options`: per-request generation overrides on chat operations, how they are carried across local, plugin and remote providers, and how providers report support.
- `output-limit-recovery`: how the agent picks the output cap, and how it detects, escalates, continues after and finally reports replies that hit the output-token limit.

### Modified Capabilities
- `acp-session-updates`: adds a requirement that runs ending on a middleware stop are reported to ACP clients through the ACP stop reason and a live-only warning notice (not replayed).

## Impact

- **querymt:**
  - `chat/mod.rs` (`ChatCall`, `ChatCapabilities`, `ChatStream`; `ChatProvider` reduced to three methods).
  - `chat/http.rs` (`HTTPChatProvider` reduced; `prepare_chat_request(call, stream)`).
  - `adapters.rs`, `tool_decorator.rs`, `validated_llm.rs`, `chain`.
  - `plugin/extism_impl` and `querymt-extism-macros` (`ExtismChatRequest.options`, reduced exports, `PREPARED_CHAT_CONTRACT_VERSION` bump).
  - All `ChatProvider` / `HTTPChatProvider` implementers under `crates/providers/`, `querymt-py`, examples and docs.
  - `chat/streaming.rs` (length-aware `finish`).
  - `querymt-remote` chat request types, builders taking `ChatCall`, host capability reporting, `MESH_PROTOCOL_VERSION` bump.
- **Providers:** Anthropic honours `max_tokens` and `reasoning_effort` per request. Other providers report no support until they are updated.
- **Agent:**
  - `model_heuristics.rs` / `session/provider_config.rs` (initial cap and ceiling).
  - `agent/execution/transitions.rs` (request options, length classification, thinking-only persistence, usage of discarded attempts).
  - `agent/execution/mod.rs` and `middleware/driver.rs` (`on_stopped`).
  - New `middleware/output_limit.rs`, plus presets and factory registration.
  - `events.rs` (`OutputLimitRecovery`, run outcome; `LlmRequestStart` unchanged).
  - `middleware/state.rs` (`ConversationContext.profile`, `LlmResponse.applied`); `agent/core.rs` (no `LastLlmRequest`).
  - `agent/session_actor.rs` (`RunCompletionDisposition::Stopped`).
  - `acp/shared.rs` (notice).
  - Regenerated typeshare types.
- **Clients:** querymt-desktop should render the notice, the `stopped` outcome and the recovery event. That is tracked separately, in the desktop repo.
- **Depends on:** `anthropic-thinking-usage` for accurate `reasoning_tokens` and adaptive effort. It can be built in parallel; the effort-lowering step only takes effect once both have landed.
