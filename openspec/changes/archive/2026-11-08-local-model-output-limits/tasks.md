# Tasks

## 1. llama.cpp per-request options

- [x] 1.1 Apply per-request `max_tokens` and `reasoning_effort` in chat and chat_stream and advertise both in capabilities; verify with `request_overrides_replace_configured_cap_and_effort`

## 2. llama.cpp thinking budget

- [x] 2.1 Add `thinking_budget` config and close reasoning in the tool, tool-streaming and thinking-streaming loops; verify with the `thinking_budget_*` tests in `common_chat.rs`
- [x] 2.2 Document `thinking_budget`, `enable_thinking` and `reasoning_effort` in docs/docs/core/available-providers.md

## 3. Stop hooks after unrecovered token-limit stops

- [x] 3.1 Run Stop hooks with `stop_reason = "ModelTokenLimit"` before publishing an unrecovered token-limit stop, sharing the continuation cap; verify with `stop_hook_can_continue_after_unrecovered_token_limit_stop`
- [x] 3.2 Document the new stop reason in docs/docs/agent/hooks.md

## Workflow follow-up

- Archive the change after review.
