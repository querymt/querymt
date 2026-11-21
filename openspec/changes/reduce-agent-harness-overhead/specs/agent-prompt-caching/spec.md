# Spec Delta

## MODIFIED Requirements

### Requirement: Default system cache hints mark only the first and last sections
When the session's provider declares explicit prompt-cache breakpoints, the agent SHALL add a default cache hint only to the first and the last system section of the resolved system prompt. Sections between them SHALL receive no default hint. A single section SHALL receive one hint. A provider that does not declare explicit breakpoints SHALL receive no default system hints. The decision SHALL depend on the provider's declaration only, never on the provider's name.

#### Scenario: Four sections get two default hints
- **WHEN** an Anthropic session's system prompt has four sections with no cache settings
- **THEN** sections 1 and 4 carry a default cache hint
- **AND** sections 2 and 3 carry no cache hint

#### Scenario: Single section gets one default hint
- **WHEN** an Anthropic session's system prompt is a single plain string
- **THEN** that section carries one default cache hint

#### Scenario: Dotagents sections count as system sections
- **WHEN** an agent with one `system` file runs in a project with dotagents `SYSTEM.md` and `AGENTS.md`
- **THEN** only the agent's file section and the `AGENTS.md` section carry default cache hints

#### Scenario: Providers without cache defaults are untouched
- **WHEN** a session uses OpenAI Chat Completions, which declares no explicit breakpoints, with three system sections
- **THEN** no system section carries a cache hint

#### Scenario: Declaration decides for an unknown provider name
- **WHEN** a session uses a provider plugin named `acme` that declares explicit breakpoints, with two system sections
- **THEN** both sections carry a default cache hint

#### Scenario: Plugin without a declaration gets no system defaults
- **WHEN** a session uses a provider plugin whose capabilities carry no prompt-cache declaration
- **THEN** no system section carries a default cache hint

### Requirement: The two most recent messages carry cache hints
Before each model request, the agent SHALL add a default cache hint to each of the last two durable messages that has none. Request-scoped context and tool-change messages SHALL NOT carry a default hint and SHALL NOT count as one of the two. Hints already present on any message SHALL be kept. Applying this twice SHALL yield the same request as applying it once.

#### Scenario: Last two messages are hinted
- **WHEN** the agent sends a request with five durable messages, no request-scoped context and no existing hints
- **THEN** only messages 4 and 5 carry a default cache hint

#### Scenario: Existing history hints are kept
- **WHEN** message 1 of five already carries a cache hint
- **THEN** messages 1, 4 and 5 carry cache hints in the request

#### Scenario: Request-scoped context is not hinted
- **WHEN** the agent sends a request with five durable messages followed by `additional_context` from a `context` hook
- **THEN** messages 4 and 5 carry a default cache hint and the trailing message carries none

#### Scenario: Tool-change message is not hinted
- **WHEN** the last durable message of a request is a tool-change message and the two before it are a tool call and its result
- **THEN** the tool call and the tool result carry a default cache hint and the tool-change message carries none

## ADDED Requirements

### Requirement: Request-scoped context follows durable history
The only messages in a request that are not in session history SHALL be request-scoped context: `additional_context` returned by `context` hooks, and the run objective when `[agent.execution] run_objective` is `true`. Request-scoped context SHALL be sent as one message after the last durable message of the request, the run objective first. It SHALL NOT be merged into a durable message, SHALL NOT be placed before one, and SHALL NOT be stored in session history.

#### Scenario: Context after a tool batch
- **WHEN** a request follows a batch of tool results and a `context` hook returns `additional_context`
- **THEN** the tool-result message is sent with exactly its stored content
- **AND** the hook's text is sent after it as the last message of the request

#### Scenario: Context after the user prompt
- **WHEN** the first request of a turn is sent with `run_objective = true`
- **THEN** the user prompt is sent with exactly its stored content
- **AND** the run objective is sent after the prompt, not before it

#### Scenario: Request without request-scoped context
- **WHEN** a request is sent with the default configuration and no `context` hook
- **THEN** its last message is the last durable message

#### Scenario: Context hook sees durable history only
- **WHEN** a `context` hook runs for a request with `run_objective = true`
- **THEN** the messages passed to the hook do not contain the run objective

#### Scenario: Context hook contribution is request-scoped
- **WHEN** a `context` hook returns `additional_context` for one request and nothing for the next
- **THEN** the second request does not contain that text
- **AND** session history does not contain it

#### Scenario: Objective and hook text share one message
- **WHEN** a request is sent with `run_objective = true` and a `context` hook returns `additional_context`
- **THEN** the request ends with one message holding the run objective followed by the hook's text

### Requirement: Durable history is sent unchanged between requests
When a session sends two consecutive model requests and its history was only appended to in between, the second request SHALL begin with the first request's durable messages, identical in content and order. Only the position of cache hints MAY differ. This SHALL hold for every provider, including providers without explicit breakpoints. The only exception is the one in "Stored tool parts are left out for providers without tool-update support".

#### Scenario: Prefix is stable across a tool step
- **WHEN** a request is answered with tool calls and the next request carries their results
- **THEN** the second request's leading messages equal the first request's durable messages

#### Scenario: Prefix is stable across turns
- **WHEN** a turn ends and the user sends a new prompt
- **THEN** the first request of the new turn begins with the durable messages of the previous turn's last request

#### Scenario: Prefix is stable when a notice is stored
- **WHEN** a progress-review notice is stored between two requests of one turn
- **THEN** the second request's leading messages equal the first request's durable messages

#### Scenario: Prefix is stable across a mode switch
- **WHEN** the user switches from build to plan mode between two requests
- **THEN** the second request's leading messages equal the first request's durable messages

#### Scenario: Prefix is stable with the run objective enabled
- **WHEN** two consecutive requests of one turn are sent with `run_objective = true`
- **THEN** the second request's leading messages equal the first request's durable messages

#### Scenario: Prefix is stable after a reload
- **WHEN** a turn in which a Stop hook continued the run ends, the session is loaded again from storage, and the user sends a new prompt
- **THEN** the first request of the new turn begins with the durable messages of the previous turn's last request, including the continuation notice

#### Scenario: Prefix is stable for a provider without breakpoints
- **WHEN** a session on a local provider that declares no explicit breakpoints sends two consecutive requests in one turn
- **THEN** the second request's leading messages equal the first request's durable messages

### Requirement: Harness notices are stored in session history
Text the agent sends to the model on its own behalf (mode reminders, output-limit guidance, progress reviews, Stop-hook continuations, the task-completion reminder, duplicate warnings, the context-usage warning and parallel-edit guidance) SHALL be stored in session history as a notice. Every request after it is stored SHALL carry it at the same position with the same content until compaction or undo removes it.

#### Scenario: Notice survives into the next turn
- **WHEN** a Stop hook continues a run with a reason, the turn ends and the user sends a new prompt
- **THEN** the first request of the new turn contains the continuation message at the position it had in the previous turn

#### Scenario: Notice is stored once
- **WHEN** a task-completion reminder is raised and the run then makes three more model requests
- **THEN** session history holds one notice with source `task_completion` and each of the three requests contains it once

### Requirement: Notices are stored at the request boundary
A notice SHALL be stored immediately before the model request that first carries it, after any pending tool results and never between a tool call and its result. A notice raised for a model request that is never sent SHALL NOT be stored.

#### Scenario: Notice follows tool results
- **WHEN** a duplicate tool-call warning is raised after a batch of two tool calls
- **THEN** the stored notice follows both tool results

#### Scenario: Notice for a request that is never sent
- **WHEN** a notice is raised and the run is cancelled before its next model request
- **THEN** session history holds no such notice

### Requirement: Notices record their source
A stored notice SHALL record its source as one of `agent_mode`, `output_limit`, `progress_review`, `stop_hook`, `task_completion`, `duplicate_tool_call`, `duplicate_code`, `parallel_edit`, `context_usage` or `background_command`. Loaded history SHALL report it as a part of kind `notice` with that `source` and its `content`.

#### Scenario: Notice is reported in loaded history
- **WHEN** a client loads a session whose history holds a parallel-edit notice
- **THEN** the loaded message has a part of kind `notice` with `source = "parallel_edit"` and the notice text

#### Scenario: Each warning kind has its own source
- **WHEN** a duplicate tool-call warning and a duplicate-code review are raised in one run
- **THEN** session history holds one notice with source `duplicate_tool_call` and one with source `duplicate_code`

### Requirement: Mode reminders are stored when the mode changes
Before each model request, the agent SHALL compare the session mode with the most recent `agent_mode` notice in the session's effective history. When the mode has a configured reminder and that notice is missing or is not this mode's reminder, the agent SHALL store this mode's reminder; otherwise it SHALL store no reminder. A mode reminder SHALL NOT be added to a request in any other way.

#### Scenario: Reminder stored once for a plan-mode run
- **WHEN** a session in plan mode with a configured plan reminder runs a turn of five model requests
- **THEN** session history holds one plan reminder, stored before the first request

#### Scenario: Switch from plan to review
- **WHEN** the user switches from plan to review mode and both have reminders
- **THEN** the review reminder is stored before the next request and no leave notice is stored

#### Scenario: Switching without a request stores nothing
- **WHEN** the user switches from build to plan to review and back to build without a model request in between
- **THEN** no mode notice is stored

#### Scenario: Reminder restored after compaction
- **WHEN** compaction removes the plan reminder while the session is in plan mode
- **THEN** the plan reminder is stored again before the next request

#### Scenario: Build session stores no mode notice
- **WHEN** a session that was never in another mode sends requests in build mode with no build reminder configured
- **THEN** session history holds no `agent_mode` notice

#### Scenario: Returning to plan mode
- **WHEN** a session left plan mode with a leave notice and the user switches back to plan mode
- **THEN** the plan reminder is stored again before the next request

### Requirement: Leaving a mode with a reminder stores a leave notice
When the session mode has no configured reminder and the most recent `agent_mode` notice in the effective history is a mode's reminder, the agent SHALL store, before the next model request, a leave notice stating that the earlier mode's instructions no longer apply. It SHALL store no leave notice in any other case.

#### Scenario: Leaving plan mode stores a leave notice
- **WHEN** the user switches from plan to build mode, build has no reminder, and the next request is sent
- **THEN** a leave notice is stored before that request and the earlier plan reminder is sent unchanged

#### Scenario: Leave notice is stored once
- **WHEN** a session that left plan mode sends three more requests in build mode
- **THEN** session history holds one leave notice

### Requirement: The run objective is sent only when enabled
`[agent.execution] run_objective` SHALL accept a boolean, default `false`. A non-boolean value SHALL be a load error naming `run_objective`. When it is `false`, no model request SHALL contain the run objective. When it is `true`, the run objective SHALL be sent as request-scoped context, without progress-review text. The `objective_initialized`, `objective_updated` and `objective_checkpoint` events SHALL be emitted in both cases.

#### Scenario: Run objective is off by default
- **WHEN** a configuration does not set `run_objective` and a turn of three model requests runs
- **THEN** none of the three requests contains the run objective

#### Scenario: Run objective enabled
- **WHEN** `[agent.execution] run_objective = true` and a turn of three model requests runs
- **THEN** each request ends with a message holding the run objective

#### Scenario: Invalid run objective setting is rejected
- **WHEN** `[agent.execution]` sets `run_objective = "auto"`
- **THEN** loading fails with an error naming `run_objective`

#### Scenario: Steering reaches the model without the objective
- **WHEN** the user steers a run with `run_objective = false`
- **THEN** the next request contains the steering text once, as the stored steering message

#### Scenario: Objective events are emitted when the objective is not sent
- **WHEN** a run is steered with `run_objective = false`
- **THEN** `objective_initialized` and `objective_updated` are emitted as they are with `run_objective = true`

### Requirement: Progress reviews are stored notices
When a run reaches its seventh model step, and every fourth step after that, the agent SHALL store one progress-review notice asking the model to review its progress against the latest instruction and applicable acceptance criteria, and stating that completion means satisfying them or reporting a real blocker. This SHALL NOT depend on `run_objective`.

#### Scenario: Progress review at step seven
- **WHEN** a run with `run_objective = false` reaches its seventh model step
- **THEN** one notice with source `progress_review` is stored before that step's request

#### Scenario: Progress review with the objective enabled
- **WHEN** a run with `run_objective = true` reaches its seventh model step
- **THEN** one progress-review notice is stored and the run objective in that request contains no progress-review text

### Requirement: Compaction keeps the active task's criteria
When compaction runs while the session has an active task with an expected deliverable or acceptance criteria, the stored summary SHALL include that task's identifier, deliverable and acceptance criteria verbatim.

#### Scenario: Compaction with an active task
- **WHEN** compaction runs while an active task has acceptance criteria "all tests pass"
- **THEN** the stored summary contains the task's identifier and the text "all tests pass"

#### Scenario: Compaction without an active task
- **WHEN** compaction runs in a session with no active task
- **THEN** the summary has no task section

### Requirement: Tool definitions are sent in a stable order
The tool definitions at the head of a model request SHALL be ordered by tool name, compared as bytes. Two requests that offer the same tools with the same definitions SHALL carry identical tool lists, across process restarts and MCP server refreshes.

#### Scenario: Same tools after a restart
- **WHEN** a session is loaded by a new agent process and offers the same tools as before
- **THEN** its first request carries the same tool list, in the same order, as the last request of the previous process

#### Scenario: MCP server refresh keeps the order
- **WHEN** an MCP server reports its tool list again with no change
- **THEN** the next request's tool list is identical to the previous request's

### Requirement: Providers declare tool-update support
A provider's chat capabilities SHALL include `tool_updates` (boolean) for the configured model and endpoint, true when tools can be added, removed and redefined by a message inside the conversation. A provider or plugin that reports no value SHALL be treated as `tool_updates = false`. The agent SHALL NOT send a tool-change message to a provider that does not declare it.

#### Scenario: Provider without the declaration
- **WHEN** a provider's capabilities carry no `tool_updates` field
- **THEN** it is treated as `tool_updates = false`

#### Scenario: Plugin built before the tool-update declaration
- **WHEN** a session uses a provider plugin built before the declaration existed and an MCP server adds a tool
- **THEN** the plugin receives the new tool in the request's tool list and no tool-change message

### Requirement: The leading tool list is a stored baseline
For a provider that declares `tool_updates`, the agent SHALL store the offered tool definitions in session history as a baseline before the first model request of a session, before the first request after a compaction and before the first request after a model change. The tool list at the head of every request SHALL be the most recent baseline.

#### Scenario: Baseline is renewed after compaction
- **WHEN** a tool was added by a tool-change message and compaction then runs
- **THEN** the first request after compaction lists that tool in its tool list and contains no tool-change message

#### Scenario: Baseline is renewed after a model switch
- **WHEN** the user switches to another model whose provider declares `tool_updates`
- **THEN** the first request to that model carries the tools offered now as its tool list

#### Scenario: Switching to a model without support and back
- **WHEN** the user switches from a model that declares `tool_updates` to one that does not, and back to the first model
- **THEN** the requests to the first model again carry the most recent stored baseline for it and the tool changes stored after it
- **AND** a new baseline is stored only when the stored one was built for another provider or model

#### Scenario: Tool list survives a restart
- **WHEN** a session with one stored tool-change message is loaded by a new agent process
- **THEN** its next request carries the same tool list and the same tool-change message as its last request before the restart

### Requirement: Tool changes are sent as messages when the provider supports them
For a provider that declares `tool_updates`, the agent SHALL compare, before each request, the tools offered now with the baseline as changed by the tool-change messages stored after it. On a difference it SHALL store one tool-change message holding the definitions added or redefined, in name order, and the names removed, and SHALL send it at that position in this and every later request until a newer baseline is stored.

#### Scenario: Tool added mid-session
- **WHEN** an MCP server adds tool `deploy` between two requests on a provider that declares `tool_updates`
- **THEN** the second request's tool list equals the first request's
- **AND** the second request contains a tool-change message that adds `deploy`

#### Scenario: Tool removed mid-session
- **WHEN** an MCP server that offered `deploy` disconnects between two requests on such a provider
- **THEN** the second request's tool list equals the first request's and a tool-change message removes `deploy`

#### Scenario: Tool definition changes
- **WHEN** the description of the `skill` tool changes between two requests on such a provider
- **THEN** a tool-change message carries the new definition of `skill` and the tool list is unchanged

#### Scenario: Unchanged tools store nothing
- **WHEN** two consecutive requests offer the same tools
- **THEN** no tool-change message is stored

#### Scenario: Tool list after undo
- **WHEN** the user undoes past a tool-change message that added `deploy` while `deploy` is still offered
- **THEN** a new tool-change message adding `deploy` is stored before the next request

### Requirement: Tool-change messages before the latest baseline are not sent
A tool-change message stored before the most recent baseline SHALL NOT be sent in any request.

#### Scenario: Tool-change messages before a newer baseline are not sent
- **WHEN** a tool-change message adding `deploy` was stored and the user then switches to another model whose provider declares `tool_updates`
- **THEN** the requests to that model carry the tools offered now as their tool list and do not contain the earlier tool-change message

### Requirement: Tool-change messages follow tool results
A baseline or tool-change message SHALL never be placed between a tool call and its result. A change found while tool results are pending SHALL be stored after them. A tool-change message SHALL be stored only when the newest conversation message is a user message, so that it directly follows one.

#### Scenario: Tool change follows tool results
- **WHEN** a tool is added while a batch of tool calls runs
- **THEN** the tool-change message is stored after the batch's tool results

#### Scenario: Nothing is stored while a tool call awaits its result
- **WHEN** a request is prepared while the newest conversation message is a tool call without a result
- **THEN** no baseline or tool-change message is stored for it
- **AND** the request carries the tools offered now as its tool list and none of the stored tool records

#### Scenario: Tool change waits for a user message
- **WHEN** the tools differ and the newest conversation message is an assistant reply without a tool call
- **THEN** no tool-change message is stored for that request
- **AND** one is stored after the next user message

### Requirement: Compaction leaves out tool records
The request that generates a compaction summary SHALL NOT contain a baseline or a tool-change message, whichever provider runs it.

#### Scenario: Compaction with stored tool records
- **WHEN** a session whose history holds a baseline and a tool change is compacted
- **THEN** the compaction request contains neither
- **AND** the compaction succeeds on a provider that does not declare `tool_updates`

### Requirement: Providers without tool-update support receive the current tool list
For a provider that does not declare `tool_updates`, the request's tool list SHALL be the tools offered now, and no baseline or tool-change message SHALL be stored.

#### Scenario: Provider without tool-update support
- **WHEN** an MCP server adds a tool between two requests on a provider that does not declare `tool_updates`
- **THEN** the second request's tool list contains the new tool and no tool-change message is stored

### Requirement: Stored tool parts are left out for providers without tool-update support
Baselines and tool-change messages already in session history, for example stored under an earlier model, SHALL stay in history unchanged. A request to a provider that does not declare `tool_updates` SHALL leave them out, neither sending nor translating them. This is the only exception to "Durable history is sent unchanged between requests"; it occurs only after a model change, which the request already reports as a prefix change.

#### Scenario: Switch to a provider without tool-update support
- **WHEN** a session whose history holds a tool-change message adding `deploy` switches to a model whose provider does not declare `tool_updates`
- **THEN** the next request contains no tool-change message, its tool list is the tools offered now, and the stored tool-change message is still in session history

### Requirement: A baseline is not a provider message
A baseline SHALL exist only as a stored session-history part. It SHALL have no form in the provider message type: the `querymt` tool-change message kind SHALL carry only the definitions added or redefined and the names removed. The messages a run holds, and the messages given to a request, a `context` hook or a compaction request, SHALL contain no baseline on any provider.

#### Scenario: Loaded history holds no baseline message
- **WHEN** a session whose history holds a baseline followed by a tool-change message is loaded
- **THEN** the loaded messages contain the tool-change message and no message for the baseline
- **AND** the baseline's tool list is the next request's tool list

#### Scenario: Tool-change message kind carries no baseline
- **WHEN** a tool-change message is serialized for a provider
- **THEN** it holds only the definitions added or redefined and the names removed

#### Scenario: Compaction drops a baseline the history no longer holds
- **WHEN** a run that holds a baseline compacts its history, and the effective history after the compaction holds no baseline
- **THEN** the run holds none either, and the next request stores a new one

### Requirement: Decisions about callable tools use the tools in effect
What depends on which tools the model can call in a request SHALL read the request's tool list with the request's tool-change messages applied, not the tool list alone. This covers the task-completion guard, the choice between a request with a tool list and one without, and the tool count reported on the model request span.

#### Scenario: Tool added by a tool change is in effect
- **WHEN** a tool is added by a tool-change message after the baseline was stored
- **THEN** the tools in effect for the request include it, and the request's tool list does not

#### Scenario: Tools offered only by a tool change
- **WHEN** the request's tool list is empty and a tool-change message in the request adds a tool
- **THEN** the request is sent with a tool list

#### Scenario: Change waiting for a user message is not in effect
- **WHEN** the tools differ and the newest conversation message is an assistant reply, so no tool-change message is stored yet
- **THEN** the tools in effect for that request do not include the added tool

### Requirement: Context hooks keep tool-change messages
A replacement history returned by a `context` hook SHALL contain exactly the tool-change messages of the history the hook was given, unchanged and in the same order. A replacement that drops, edits or reorders one SHALL be ignored with a notice, and the request SHALL be built from the history the hook was given.

#### Scenario: Replacement drops a tool-change message
- **WHEN** a `context` hook returns a replacement history without a tool-change message it was given
- **THEN** the replacement is ignored, a notice names tool-change messages, and the request contains the tool-change message

#### Scenario: Replacement keeps the tool-change messages
- **WHEN** a `context` hook returns a replacement history that rewrites other messages and keeps every tool-change message in place
- **THEN** the replacement is used and no notice is emitted

### Requirement: Providers declare prompt-cache support
A provider's chat capabilities SHALL include a `prompt_cache` declaration for the configured model and endpoint with two fields: `explicit_breakpoints` (boolean) and `max_ttl_secs` (the longest cache lifetime in seconds the provider accepts; absent when the lifetime cannot be chosen). A provider or plugin that reports no declaration, or a declaration without a field, SHALL be treated as `explicit_breakpoints = false` with no selectable lifetime.

#### Scenario: Anthropic declaration
- **WHEN** the capabilities of an Anthropic provider are read
- **THEN** `prompt_cache.explicit_breakpoints` is true and `prompt_cache.max_ttl_secs` is 3600

#### Scenario: OpenAI Responses declaration follows the model
- **WHEN** the capabilities of an OpenAI Responses provider are read for a model that supports cache breakpoints
- **THEN** `prompt_cache.explicit_breakpoints` is true and `prompt_cache.max_ttl_secs` is 1800

#### Scenario: Model without breakpoint support
- **WHEN** the capabilities of an OpenAI Responses provider are read for a model that does not support cache breakpoints
- **THEN** `prompt_cache.explicit_breakpoints` is false

#### Scenario: Plugin built before the declaration existed
- **WHEN** a provider plugin returns capabilities without a `prompt_cache` field
- **THEN** the plugin loads and is treated as declaring no explicit breakpoints and no selectable lifetime

#### Scenario: Two providers with the same configuration text
- **WHEN** a provider that declares explicit breakpoints and one that declares none are configured with the same model and system prompt, so that their configurations are the same text
- **THEN** each gets default hints according to its own declaration

### Requirement: Prompt cache settings are validated
`[agent.execution.prompt_cache]` SHALL accept `ttl_secs` (integer seconds, 0 to 86400, default 0, where 0 means the provider's default lifetime). A value outside the range, a non-integer value or an unknown key in the table SHALL be a load error naming the key.

#### Scenario: Prompt cache settings default when omitted
- **WHEN** a configuration has no `[agent.execution.prompt_cache]` table
- **THEN** it loads with `ttl_secs = 0`

#### Scenario: Out-of-range lifetime is rejected
- **WHEN** `[agent.execution.prompt_cache]` sets `ttl_secs = 100000` or `ttl_secs = -1`
- **THEN** loading fails with an error naming `ttl_secs`

#### Scenario: Unknown prompt cache key is rejected
- **WHEN** `[agent.execution.prompt_cache]` sets `keep_alive = true`
- **THEN** loading fails with an error naming `keep_alive`

### Requirement: Default hints request the configured lifetime
When `ttl_secs` is greater than 0 and the provider declares `max_ttl_secs`, every default cache hint the agent adds, on system sections and on messages, SHALL request `min(ttl_secs, max_ttl_secs)` seconds. When the provider declares no `max_ttl_secs`, or `ttl_secs` is 0, default hints SHALL request no lifetime. Hints set explicitly in configuration SHALL keep their own lifetime.

#### Scenario: Requested lifetime is within the provider's maximum
- **WHEN** `ttl_secs = 3600` and the provider declares `max_ttl_secs = 3600`
- **THEN** every default hint requests 3600 seconds

#### Scenario: Requested lifetime exceeds what the provider offers
- **WHEN** `ttl_secs = 3600` and the provider declares `max_ttl_secs = 1800`
- **THEN** every default hint requests 1800 seconds

#### Scenario: Provider without a selectable lifetime
- **WHEN** `ttl_secs = 3600` and the provider declares no `max_ttl_secs`
- **THEN** default hints request no lifetime and the request is sent

#### Scenario: Default lifetime
- **WHEN** `ttl_secs` is 0
- **THEN** default hints request no lifetime

#### Scenario: Explicit hint lifetime wins
- **WHEN** `ttl_secs = 3600` and a system section is configured with `cache = { ephemeral = { ttl_seconds = 300 } }`
- **THEN** that section's hint requests 300 seconds

### Requirement: Providers round a requested lifetime up
A provider SHALL send a requested cache lifetime, on a default or an explicit hint, as the shortest lifetime it supports that is at least the requested one. A request for more than the longest lifetime it supports SHALL fail with an error that names the lifetime, except where the provider declares no selectable lifetime and sends no lifetime.

#### Scenario: Anthropic rounds up
- **WHEN** a hint requests 600 seconds on an Anthropic provider
- **THEN** the request carries the 1-hour lifetime
- **AND** a hint requesting 120 seconds carries the 5-minute lifetime

#### Scenario: OpenAI Responses rounds up to 30 minutes
- **WHEN** a hint requests 600 seconds on an OpenAI Responses provider
- **THEN** the request carries the 30-minute lifetime

#### Scenario: OpenAI Responses rejects a longer lifetime
- **WHEN** a hint requests 3600 seconds on an OpenAI Responses provider
- **THEN** the request fails with an error naming 3600

### Requirement: Cache prefix changes are attributed
For every main model request the agent SHALL record `prefix_change` and `idle_secs` in the request-end event, and as `qmt.cache.prefix_change` and `qmt.cache.idle_secs` on the request's trace span. `prefix_change` SHALL be `first_request`, `model`, `tools`, `system`, `history` or `none`; when several apply, the first in that order. `idle_secs` SHALL be the whole seconds since the session's previous main model request ended, and absent for `first_request`.

#### Scenario: Unchanged prefix
- **WHEN** a request follows another with the same model, tools and system prompt and only appended history, 12 seconds later
- **THEN** it records `prefix_change = "none"` and `idle_secs = 12`

#### Scenario: Model switch
- **WHEN** the session's model is changed between two requests
- **THEN** the second request records `prefix_change = "model"`

#### Scenario: Offered tools change
- **WHEN** an MCP server adds a tool between two requests of the same model on a provider that does not declare `tool_updates`
- **THEN** the second request records `prefix_change = "tools"`

#### Scenario: Tool change sent as a message
- **WHEN** an MCP server adds a tool between two requests on a provider that declares `tool_updates`, with nothing else changed
- **THEN** the second request records `prefix_change = "none"`

#### Scenario: History is rewritten
- **WHEN** compaction, pruning or undo replaces or removes durable messages between two requests
- **THEN** the second request records `prefix_change = "history"`

#### Scenario: First request of a loaded session
- **WHEN** the agent sends its first main model request for a session after loading it
- **THEN** the request records `prefix_change = "first_request"` and no `idle_secs`

#### Scenario: Long wait with an unchanged prefix
- **WHEN** a request follows the previous one after 30 minutes with nothing else changed
- **THEN** it records `prefix_change = "none"` and `idle_secs = 1800`
