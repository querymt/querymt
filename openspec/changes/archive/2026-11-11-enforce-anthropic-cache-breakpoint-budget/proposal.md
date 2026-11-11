# Proposal

## Why

Anthropic accepts at most four `cache_control` breakpoints per request and rejects a request with more. QueryMT never counts them. The agent puts a cache hint on every system section and on the last two messages, and the Anthropic provider sends each hint as a breakpoint. Three or more system sections therefore make the request fail with a 400 error. That happens with no unusual config: an agent `system` file plus dotagents `SYSTEM.md` and `AGENTS.md` is already three, and a delegation planning summary adds a fourth. Mixed lifetimes can also put a one-hour breakpoint after a five-minute one, which Anthropic rejects as well.

## What Changes

- The Anthropic provider sends at most four cache breakpoints per request. If the hints it receives would produce more, it keeps the ones that preserve the most reuse:
  - the last system breakpoint;
  - the newest message breakpoints;
  - the first system breakpoint.
  It drops the rest and logs one warning with the number dropped. Breakpoints are counted only where a marker is actually emitted, so hints on blocks that cannot carry one do not use up the budget.
- The Anthropic provider makes breakpoint lifetimes non-increasing in request order. An earlier breakpoint is raised to the longest lifetime that follows it, so a one-hour breakpoint never follows a five-minute one.
- **BREAKING (agent cache defaults):** for Anthropic, Codex and OpenAI Responses, the agent now adds default cache hints only to the first and last system sections, not to every section.
  - Explicit hints on any section are kept unchanged.
  - Middle sections are still cached, because the last section's prefix covers them. They no longer get their own reuse boundary.
  - Default placement is therefore at most two system breakpoints plus the agent's two message breakpoints: four in total.
- The agent continues to add hints to the last two messages. Hints that are already present earlier in history are kept and left for the provider budget to resolve.
- Docs: the Prompt Cache Hints section describes the first/last rule, the four-breakpoint limit and the drop order. CHANGELOG gets an Unreleased entry.

## Non-goals

- No cache breakpoints on tool definitions. The first system breakpoint already covers the tool prefix.
- No new configuration key for a cache policy (auto/none/custom). The existing per-section `cache` hint and native `cache_control` stay the only controls.
- No breakpoint limit for OpenAI Responses or Codex. Only the Anthropic wire format gets a provider cap. The narrower agent defaults apply to all three providers.
- No change to OpenRouter or other gateways that forward to Anthropic models.
- No UI work.

## Capabilities

### New Capabilities
- `agent-prompt-caching`: where the agent runtime places default prompt-cache hints on configured system sections and on recent conversation messages.

### Modified Capabilities
- `anthropic-messages-protocol`: adds requirements for the request-wide limit of four cache breakpoints, the drop priority, and the lifetime ordering of breakpoints.

## Impact

- **Packages:**
  - `qmt-anthropic`: request building in `build_chat_request`. The code is shared by the `native` and `extism` builds, so the wasm32-wasip1 plugin is affected.
  - `querymt-agent`: `model_heuristics` system cache defaults; no feature-gated code.
  - Downstream of `qmt-anthropic`: `querymt-mobile-ffi`, which only needs to rebuild.
- **Provider wire shape:** Anthropic requests can now carry fewer `cache_control` markers than the hints supplied, and an earlier marker can carry a longer `ttl` than its hint asked for. Requests that used to fail with a 400 now succeed.
- **Agent behaviour (BREAKING):** middle system sections no longer get a default cache hint. To keep a boundary on such a section, configure an explicit `cache` hint on it. If that pushes a request over four breakpoints, the provider budget applies.
- **Unchanged:** public Rust API, TOML/JSON config keys, hooks, middleware, ACP, the Extism contract, the session DB, Python bindings and typeshare types.
- **UI follow-up:** none required in querymt-desktop.
