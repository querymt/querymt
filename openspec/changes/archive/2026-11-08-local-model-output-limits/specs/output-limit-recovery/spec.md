# Spec Delta

## MODIFIED Requirements

### Requirement: Middleware can recover from stops
Before a run stop is published, the agent SHALL give middleware the chance to replace it with a continuation. After at most a configurable number of such continuations per turn (default 5), the stop SHALL be published unchanged, except that a model-token-limit stop SHALL first be offered to Stop hooks as described in "Stop hooks can continue after an unrecovered token-limit stop".

#### Scenario: Middleware continues
- **WHEN** a stop occurs and a middleware returns a continuation
- **THEN** no stop event is emitted and the run continues with the next model request

#### Scenario: Guard reached
- **WHEN** middleware has already continued the maximum number of times in the turn
- **THEN** a model-token-limit stop is first offered to Stop hooks, and a blocking hook continues the run as in "Stop hooks can continue after an unrecovered token-limit stop" only when `max_stop_hook_continuations` is neither zero nor exhausted for the turn; otherwise the stop is published
- **AND** any other stop is published unchanged

## ADDED Requirements

### Requirement: Stop hooks can continue after an unrecovered token-limit stop
When a model-token-limit stop is not recovered by middleware, the agent SHALL run Stop hooks with stop reason `ModelTokenLimit` before publishing it. A hook that blocks SHALL continue the turn with its reason injected as a runtime reminder. Token-limit continuations SHALL be counted separately from end-of-turn continuations, both bounded by `[agent.execution] max_stop_hook_continuations` (integer, default 1; `0` = advisory).

#### Scenario: Hook continues the run
- **WHEN** a token-limit stop is not recovered and a Stop hook blocks
- **THEN** no stop event is emitted and the run continues with the next model request

#### Scenario: Cap reached
- **WHEN** token-limit Stop-hook continuations have already used the cap in the turn
- **THEN** the token-limit stop is published

### Requirement: Local providers can bound reasoning per reply
A local provider with a configured reasoning budget SHALL close an open reasoning block once that many tokens were generated inside reasoning in one reply, so the remaining output cap is used for the answer. The budget is the llama.cpp provider's `thinking_budget` option in `[providers.config]` (integer; unset = unbounded; not applied with `speculative` decoding, when `json_schema` constrains the output, or to plain non-streaming `chat()` calls that use `generate()`). The effective budget is reduced to fit within the output cap, leaving room for the end-of-reasoning marker and at least one answer token, so a configured budget of 4,096 does not guarantee 4,096 reasoning tokens (with the default output cap of 256 it allows fewer than 256); when the cap cannot fit the marker and one answer token, the budget is disabled. Without a budget, reasoning SHALL be unbounded.

#### Scenario: Budget spent while reasoning
- **WHEN** the reasoning budget is 4,096 and the model is still reasoning after 4,096 reasoning tokens
- **THEN** the provider inserts the end-of-reasoning marker and generation continues as answer text

#### Scenario: Reasoning ends within budget
- **WHEN** the model closes its reasoning before the budget is spent
- **THEN** nothing is inserted
