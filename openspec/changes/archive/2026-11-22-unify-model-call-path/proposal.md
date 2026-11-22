# Proposal

## Why

A run asks the same questions about its model several times per request, and a compaction request takes a different road from a model request. Both were found by drawing the call graph of one request in `querymt-agent-runtime` after `reduce-agent-harness-overhead`:

- **A provider's capabilities are read again for every use.** One model request reads them four times: twice while the request is prepared, once to choose streaming (or once to filter options on the path without tools), and once in the HTTP adapter's check for tool-change messages. For an Extism provider plugin every read locks the plugin, copies its configuration and calls into Wasm; nothing keeps the answer, although it cannot change after the provider is built. The run already holds the answer in its model profile and only compares it with a fresh read.
- **The run's provider was looked up again at every step.** When this change was written, the request-preparation step and the call step each resolved the provider through their own retry wrapper (three places), because the turn started with a profile that might be empty. `restructure-agent-turn-state` task 6.2 has since resolved the provider once, before the first hook, and removed the three lookups. What is left is that the model is held in three places (`Run`, `Turn::profile`, the session handle's cache), is read by `/compact` through its own path, and has its capabilities read from the provider again for every request.
- **A reply's replay identity is guessed from history.** `SessionHandle::output_target` completes the protocol and endpoint a stored reply is replayed against from the latest stored output of the same provider and model, because the session store does not record them. The provider that produced the reply knows them.
- **Compaction has its own retry loop.** It waits a fixed backoff, ignores the per-error waits and `transport_wait_secs` that `reduce-agent-harness-overhead` gave model requests, reports no attempt on its span, cannot be cancelled while it waits, and retries errors that cannot succeed (a rejected credential, an invalid request) as often as transient ones. Every change to request retries has to be made twice or is missed there, as the attempt reporting was.

Why now: `reduce-agent-harness-overhead` added reads to the request path (`tool_updates`, the prompt-cache declaration) and `restructure-agent-turn-state` is about to give run state one owner. Resolving the provider once is a precondition for its call stage.

## What Changes

The groups can land separately, in this order (groups A and B are partly done: see each).

**A. Capabilities are read once per built provider**

- An Extism provider reads its plugin's capabilities at most once and keeps the answer for its lifetime. A read that fails is not kept, so a later read tries again.
- The run reads capabilities from the model it resolved (B), not from the provider, at every place in the run loop. A compaction, which may use another provider, reads that provider's capabilities once.
- No change to what any provider declares.

**B. A run resolves its model once and `TurnEnv` carries it**

- The provider and its profile (capabilities, output caps, configured effort) are resolved together, once, by one `resolve_model`, with the retry rules provider setup has, after the user's prompt is stored and before the run's environment (`TurnEnv`) is built. The resolved model is a field of `TurnEnv`. `Run::provider`, `Turn::profile` and `SessionHandle::profile` are removed; no step resolves the provider or repairs the profile, and no hook can see an empty one.
- Already landed by `restructure-agent-turn-state` 6.2, and not repeated here: the provider is resolved before the first hook, the failure is the provider-initialization error, and the three per-step resolutions and `supported_overrides` are gone. This group writes the missing CHANGELOG and documentation entry for it.
- Capabilities (`streaming`, `tool_updates`, the prompt-cache lifetime) are read from the resolved model's profile at every place in the run loop and in compaction.
- **Behaviour changes**: native `/compact` with an unbuildable provider fails before the PreCompaction hook and the `CompactionStart` event, with the same provider failure (today both are emitted first). A run whose model cannot be resolved takes no pre-turn snapshot. The `agent.execution.provider_init` span is a child of the prompt's span, not of `agent.execution.turn`. The user's prompt is still stored before a provider failure, and the events a client sees for a failed run are unchanged.
- No public item changes in this group. The crate-internal `SessionHandle::profile` is removed.
- Offered tools (built-in and MCP) are not part of the resolved model: they are collected for every request, so an MCP server's `tools/list_changed` still takes effect between the requests of one run.

**C. The replay identity of a reply comes from its provider**

- `ChatProvider` gets a defaulted `output_provenance` that the OpenAI, xAI and Codex providers implement from the values they already stamp on their output; the resolved model keeps it and the request projection reads it, instead of completing the identity from stored history. Providers that cannot say (Extism, mesh) keep the history completion. No contract change: a plugin built before this change behaves as before.

**D. Compaction requests follow the retry rules of model requests**

- A provider error during a compaction request is retried by the rules of a model request: only retryable errors, the wait the error class asks for, the attempt limit of `[agent.execution.rate_limit]`, cancellable while waiting, with the same retry notices to clients and one `gen_ai.request.attempt` event per attempt on the compaction's request span.
- A summary the agent rejects (empty, refused, cut off) is still retried by compaction's own budget, `[agent.execution.compaction.retry]`.
- **Behaviour change** (config semantics): `[agent.execution.compaction.retry]` `max_retries`, `initial_backoff_ms` and `backoff_multiplier` apply to rejected summaries only. Provider errors during compaction use `[agent.execution.rate_limit]`. A compaction that hits a non-retryable provider error fails at once; today it is retried `max_retries` times.
- A request timeout still ends the compaction without a retry, as today (the remote side is probably still working on the request).
- Both callers of the compaction retry use it: a run (and native `/compact`), and the handoff summary of a worktree switch, which has no run and cannot be cancelled.

## Non-goals

- **Moving the system prompt from the provider configuration to the request.** It would let default cache hints for the system prompt be applied when a request is built, and remove the provider build that only reads the prompt-cache declaration (`SessionProvider::with_default_cache_hints`) and its cache. It is left out: twelve provider configurations type `system` differently, the Extism request contract would need a new field, and a plugin built before the change would silently send no system prompt unless a capability flag keeps the configuration path alive beside the new one, so nothing could be deleted until old plugins are no longer supported. Design "D5" records the analysis and when to revisit it.
- Reading capabilities for a configuration without building a provider. It would save the build above for Extism providers only, and the declaration cache would stay.
- Carrying the replay identity across the Extism or mesh boundary. It needs a plugin-contract field, which this change and `stabilize-native-plugin-abi` exclude.
- Retrying the provider build for `/compact`. It has no retry today.
- Giving the delegation summarizer retries. It sends one request and has none today.
- Changing what a compaction request contains or how a summary is validated. `restructure-agent-turn-state` builds its messages with the shared request projection.
- Changing provider wire shapes, the Extism plugin contract, the native plugin interface, ACP payloads, the session DB schema or Python bindings.
- Caching capabilities in native providers. Their `capabilities` is a field read or a match on the model id.

## Capabilities

### New Capabilities

- `provider-resolution`: how a run obtains its provider and that provider's capabilities: resolved once per run after its prompt is stored and before its first middleware hook, and capabilities read from a plugin once per built provider.
- `session-compaction`: how a compaction request is sent: which failures are retried and by which budget, how attempts are reported, cancellation.

### Modified Capabilities

None. `chat-generation-options` and `agent-prompt-caching` keep their requirements: what is sent does not change.

## Impact

**Packages**

- `querymt`: `plugin/extism_impl/host/mod.rs` (`ExtismProvider` keeps its capabilities; done), and for group C `chat/mod.rs` (`ChatProvider::output_provenance`) with the forwarding wrappers `ValidatedLLM`, `ToolEnabledProvider`, `LLMProviderFromHTTP`. Feature whose code changes: `extism_host`. Public API change in group C only: one defaulted trait method.
- Provider crates (group C): `qmt-openai`, `qmt-xai`, `qmt-codex`.
- `querymt-agent-runtime`: `session/provider.rs` (`SessionHandle`, `ProviderSnapshot`, `ResolvedModel`), `agent/execution/{stage,turn,transitions,llm_retry,maintenance,projection}.rs`, `agent/session_actor.rs` (`execute_prompt_inner`, where `TurnEnv` is now built after the model), `agent/repository_delivery.rs` (`orientation_contexts` stops taking a `TurnEnv`), `agent/handle/ext_worktree_ops.rs` (second caller of the compaction retry), `session/compaction.rs`, `delegation/summarizer.rs`, `test_utils/fixtures.rs`. `RemoteChatProvider` and `MeshChatProvider` (group C forwarding) are behind `remote`. No other gated code path contains run-loop code.
- `querymt-agent`: facade and examples, build check only.
- `querymt-agent-py`, `querymt-mobile-ffi`: build check only.

**Contracts**

- Public Rust API: none changes. `ModelProfile` and `ConversationContext::profile` keep their shape.
- Config: no key is added or removed. **Behaviour change** in what `[agent.execution.compaction.retry]` governs (group D).
- Middleware behaviour: turn-start hooks never see a profile without capabilities, and do not run when the provider cannot be built (already so since `restructure-agent-turn-state` 6.2; now documented and structural).
- ACP-visible behaviour (group B): native `/compact` with an unbuildable provider ends with the provider failure before any compaction event.
- Telemetry: compaction request spans gain `gen_ai.request.attempt` and its events (group D); `agent.execution.provider_init` moves under the prompt's span (group B).
- ACP, hooks, provider wire shapes, Extism contract, native plugin interface, session DB schema, Python bindings, typeshare: no change.

**querymt-desktop follow-up (not planned here):** confirm that retry notices during a compaction render like those of a model request.

**Docs:** `docs/docs/agent/configuration.md` (`[agent.execution.compaction.retry]` and `[agent.execution.rate_limit]`), `docs/docs/telemetry.md` (attempts on compaction spans; the `provider_init` span's parent), `docs/docs/agent/middleware.md` (turn-start hooks and the profile), `crates/agent-runtime/knowledge/execution.md`, CHANGELOG "Unreleased".

**In-flight changes to coordinate with:** `restructure-agent-turn-state` has landed (`TurnEnv`, `Turn`, `Run`, `Stage`); group B moves the model it left in `Run` onto `TurnEnv` (its design "D2", tasks 6.2 and 5.7, whose references to this change's "group 2" and "task 2.4a" now mean groups B and C). `reduce-agent-harness-overhead` owns the retry waits and attempt reporting that group C reuses; start group D after its group 5 and task 9.3 have landed (both are done). `stabilize-native-plugin-abi` does not interact: no plugin-facing trait changes here.
