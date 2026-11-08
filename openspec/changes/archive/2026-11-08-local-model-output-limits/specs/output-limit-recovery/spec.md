# Spec Delta

## MODIFIED Requirements

### Requirement: Middleware can recover from stops
Before a run stop is published, the agent SHALL give middleware the chance to replace it with a continuation. After at most a configurable number of such continuations per turn (default 5), the stop SHALL be published unchanged, except that a model-token-limit stop SHALL first be offered to Stop hooks as described in "Stop hooks can continue after an unrecovered token-limit stop".

#### Scenario: Middleware continues
- **WHEN** a stop occurs and a middleware returns a continuation
- **THEN** no stop event is emitted and the run continues with the next model request

#### Scenario: Guard reached
- **WHEN** middleware has already continued the maximum number of times in the turn
- **THEN** the stop is published

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
A local provider with a configured reasoning budget SHALL close an open reasoning block once that many tokens were generated inside reasoning in one reply, so the remaining output cap is used for the answer. The budget is the llama.cpp provider's `thinking_budget` option in `[providers.config]` (integer; unset = unbounded, not applied with `speculative` decoding). Without a budget, reasoning SHALL be unbounded.

#### Scenario: Budget spent while reasoning
- **WHEN** the reasoning budget is 4,096 and the model is still reasoning after 4,096 reasoning tokens
- **THEN** the provider inserts the end-of-reasoning marker and generation continues as answer text

#### Scenario: Reasoning ends within budget
- **WHEN** the model closes its reasoning before the budget is spent
- **THEN** nothing is inserted
