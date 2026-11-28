# Spec Delta

## Purpose

Keeps the conversation history a run sends to the model identical to the history stored for the session, so that reloading a session never changes what the model has already seen. A run holds the stored messages, and one projection turns them into what a provider is sent.

## ADDED Requirements

### Requirement: History sent in a run matches the stored session
At every model request of a run, the durable messages sent SHALL equal, in order and content, the messages that loading the stored session at that moment and preparing the same request would send. Cache hints and the request-scoped tail are not part of the comparison.

#### Scenario: Request after a tool step matches a reload
- **WHEN** a run stores an assistant reply with tool calls and their results and prepares the next model request
- **THEN** the durable messages of that request equal those of a request prepared from a fresh load of the session

#### Scenario: Request after a reply with provider reasoning matches a reload
- **WHEN** a run stores an assistant reply that carries provider-native reasoning (a signature or encrypted content) and tool calls, stores their results and prepares the next model request for the same provider and model
- **THEN** the durable messages of that request, the native reasoning included, equal those of a request prepared from a fresh load of the session

#### Scenario: Request after a stored tool change matches a reload
- **WHEN** a tool baseline and then a tool change are stored for a provider that takes tool changes as messages
- **THEN** the durable messages and the tool list of the next request equal those of a request prepared from a fresh load of the session

#### Scenario: Request after a stored notice matches a reload
- **WHEN** a notice is stored before a model request
- **THEN** the durable messages of that request equal those of a request prepared from a fresh load of the session

#### Scenario: Request after a mid-run compaction matches a reload
- **WHEN** history is compacted in the middle of a run and the run prepares its next model request
- **THEN** the durable messages of that request equal those of a request prepared from a fresh load of the session

#### Scenario: First request of the next run continues the previous one
- **WHEN** a run completes without end-of-run pruning changing any message, and the session starts another run
- **THEN** the durable messages of the new run's first request begin with the durable messages of the previous run's last request

### Requirement: One projection builds every request from the stored messages
The messages of a model request and of a compaction request SHALL be produced from the session's stored messages by the same projection, which SHALL be the only place that decides how a stored part is sent. For the same stored messages and the same target (provider, protocol, model, endpoint, prompt size limit, and whether the provider takes tool changes as messages) it SHALL produce the same messages.

#### Scenario: Notice is sent as user text
- **WHEN** the stored history holds a notice
- **THEN** a model request and a compaction request both carry its text as user text at its position

#### Scenario: Baseline is the tool list and never a message
- **WHEN** the stored history holds a tool baseline and the request is for a provider that takes tool changes as messages
- **THEN** the request's tool list is the baseline's and no message of the request represents the baseline

#### Scenario: Tool changes follow the provider's declaration
- **WHEN** the stored history holds a tool change after the baseline in effect
- **THEN** a request for a provider that takes tool changes as messages contains it at its position
- **AND** a request for a provider that does not, and a compaction request, do not contain it

#### Scenario: Native output is replayed only to its origin
- **WHEN** the stored history holds an assistant reply with provider-native reasoning
- **THEN** a request for the provider and model that produced it carries the native form
- **AND** a request for another model carries the portable form

### Requirement: State read from the history follows the history
What a run reads from its history (the tool baseline in effect, the most recent mode reminder, the number of user turns) SHALL be what the stored messages hold at that moment, including after a compaction or an undo in the same run.

#### Scenario: Mode reminder compacted away is stored again
- **WHEN** a compaction removes the stored mode reminder and the session is still in that mode
- **THEN** the next request of the same run stores the reminder again

#### Scenario: Baseline compacted away is stored again
- **WHEN** a compaction removes the stored tool baseline on a provider that takes tool changes as messages
- **THEN** the next request of the same run stores a new baseline and leads with the tools offered now

### Requirement: Token estimates of a history agree
Every token estimate the agent makes of a session's history SHALL be computed by the same estimate over the projected request: the request budget, the figure that starts a compaction, the before and after figures of a compaction, and the delegation summary budget. The estimate SHALL count every part the request sends, including steering, reasoning, compaction summaries, tool-change definitions and the tool list.

#### Scenario: Compaction figure equals the request estimate
- **WHEN** a history holds steering messages, reasoning and a tool change, and a compaction starts
- **THEN** the token count reported at the start of the compaction equals the estimate of a model request prepared from the same history without its tail

#### Scenario: Tool-change definitions are counted
- **WHEN** a stored tool change adds a tool whose definition is 4,000 bytes
- **THEN** every estimate of that history grows by the estimate of that definition

### Requirement: Skipped tool results are stored before the steering that skipped them
When steering is applied before a tool batch starts, every call of the batch SHALL get one skipped result, and the message holding those results SHALL be stored directly after the assistant message that made the calls and before the steering message. The stored order SHALL be the order sent to the model.

#### Scenario: Steering arrives before a tool batch
- **WHEN** the model requests two tool calls and steering is applied before either runs
- **THEN** the stored history holds the assistant message, then one message with two skipped results, then the steering message
- **AND** the next model request sends them in that order

#### Scenario: Steering arrives while tools run
- **WHEN** steering is accepted while a tool batch is running
- **THEN** the batch finishes, its results are stored directly after the assistant message, and the steering message is stored after them before the next model request

### Requirement: A message that cannot be stored is not sent
When storing a message fails, the run SHALL fail with that error, and no later model request of that run SHALL contain the message.

#### Scenario: Storing an assistant reply fails
- **WHEN** the store rejects the write of an assistant reply
- **THEN** the run fails and no further model request is sent in that run
