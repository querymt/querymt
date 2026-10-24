# Proposal

## Why

When a model reply hits its output-token cap (`finish_reason = Length`), the agent ends the run straight away. It calls this `Stopped{ModelTokenLimit}`, reports the run as `completed`, and clients get no notice. Session `01a16d18-6bf3-7219-849e-967cc8b81124` ended this way after one reply used all 32,000 output tokens on thinking. Three things made it worse:

- The cap is a fixed 32k for every Anthropic model (`model_heuristics.rs`), although the model allowed 128k.
- No middleware can react, because `Stopped` never goes through the middleware pipeline.
- Two earlier replies that were probably also cut off mid-tool-call were retried as transient stream errors, with 50–55 s waits and the partial output thrown away.

## What Changes

- **Per-request generation options in querymt.** Chat calls accept optional per-request settings (`max_tokens`, `reasoning_effort`) that override the provider's configured values for that one request. They are carried by the HTTP adapter, the Extism plugin interface and the remote mesh. Providers declare whether they honour them. The Anthropic provider is the first to do so.
- **Starting `max_tokens` from model metadata.** When no `max_tokens` is configured, the cap is `min(32,000, model output limit)`, and the model output limit is recorded as the escalation ceiling. This replaces the fixed Anthropic 32k heuristic. An explicit `max_tokens` is always respected and turns escalation off.
- **Cut-offs become length outcomes.** A stream that ends with `Length` and unfinished tool calls is treated as an incomplete, length-limited reply, not a transient error. It is no longer retried, and its partial output is kept.
- **New `on_stopped` middleware phase.** It runs before a stop is published. Middleware may turn the stop into `BeforeLlmCall` to keep going, with a per-turn loop guard.
- **New `OutputLimitRecoveryMiddleware`** (`type = "output_limit_recovery"`, on by default):
  - In `on_after_llm`, it retries a cut-off reply once at the escalation ceiling and discards the cut-off reply.
  - In `on_stopped`, it continues up to N times (default 3) with a configurable fixed instruction: resume without recap, split large writes into smaller tool calls. For thinking-only cut-offs it also lowers reasoning effort for that request.
  - The instruction is removed after the next reply that isn't cut off.
- **Accounting.** Discarded attempts still count towards usage and cost. Thinking-only cut-offs are not stored in history.
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
  - `chat/mod.rs` (`ChatProvider` gains option-aware methods with defaults).
  - `chat/http.rs` (`prepare_chat_request` with options).
  - `adapters.rs`.
  - `plugin/extism_impl` (`ExtismChatRequest.options`, serde default).
  - `chat/streaming.rs` (length-aware `finish`).
  - `querymt-remote` chat request types.
- **Providers:** Anthropic honours `max_tokens` and `reasoning_effort` per request. Other providers report no support until they are updated.
- **Agent:**
  - `model_heuristics.rs` / `session/provider_config.rs` (initial cap and ceiling).
  - `agent/execution/transitions.rs` (request options, length classification, thinking-only persistence, usage of discarded attempts).
  - `agent/execution/mod.rs` and `middleware/driver.rs` (`on_stopped`).
  - New `middleware/output_limit.rs`, plus presets and factory registration.
  - `events.rs` (`OutputLimitRecovery`, run outcome).
  - `agent/session_actor.rs` (`RunCompletionDisposition::Stopped`).
  - `acp/shared.rs` (notice).
  - Regenerated typeshare types.
- **Clients:** querymt-desktop should render the notice, the `stopped` outcome and the recovery event. That is tracked separately, in the desktop repo.
- **Depends on:** `anthropic-thinking-usage` for accurate `reasoning_tokens` and adaptive effort. It can be built in parallel; the effort-lowering step only takes effect once both have landed.
