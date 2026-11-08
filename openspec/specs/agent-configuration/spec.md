# agent-configuration Specification

## Purpose

Defines how an agent's initial mode and middleware entries become runtime settings, identically for single agents, quorum planners and quorum delegates, and guarantees that invalid values fail the build instead of being ignored.

## Requirements

### Requirement: Initial session mode is a typed agent setting
`[agent]`, `[planner]` and each `[[delegates]]` entry SHALL accept a `mode` key with value `build`, `plan` or `review`, defaulting to `build`. New sessions of that agent SHALL start in the configured mode. An unknown value MUST fail the config load with an error naming the field.

#### Scenario: Single agent starts in plan
- **WHEN** `[agent] mode = "plan"` and a new session is created
- **THEN** the session's mode is `plan`

#### Scenario: Delegate starts in review
- **WHEN** a delegate sets `mode = "review"` and the planner delegates to it
- **THEN** the delegate's session starts in `review`

#### Scenario: Mode typo in a quorum
- **WHEN** a quorum planner or delegate sets `mode = "plna"`
- **THEN** loading fails with an error naming the `mode` field and the valid values

### Requirement: Agent mode middleware only configures reminders
The `agent_mode` middleware entry SHALL configure mode reminders only and MUST reject unknown keys, including the former `default` key.

#### Scenario: Leftover default key
- **WHEN** an `agent_mode` middleware entry sets `default = "plan"`
- **THEN** building the agent fails with an error naming the unknown `default` key

### Requirement: Middleware errors fail the build
Building a single agent, quorum planner or quorum delegate SHALL fail when a configured middleware entry has an unknown type or invalid configuration. A middleware entry that is explicitly turned off by its own configuration SHALL be skipped without error. No failure MAY be skipped based on the text of its error message.

#### Scenario: Invalid delegate middleware
- **WHEN** a quorum delegate configures a `limits` middleware with an invalid value
- **THEN** building the quorum fails with an error naming the delegate and the middleware

#### Scenario: Middleware turned off
- **WHEN** a `context` middleware entry is configured with its enable flag off
- **THEN** the agent builds and that middleware is not installed

#### Scenario: Same behavior for single agent and quorum
- **WHEN** the same invalid middleware entry is used in a single-agent config and in a quorum planner config
- **THEN** both builds fail with the same error

### Requirement: Selecting the profile's model keeps the profile's parameters
When a model selection names the profile's provider and model exactly, the configuration stored for the session SHALL be the profile's configuration, with the session's system prompt and reasoning effort. A selection of any other provider or model SHALL NOT include the profile's parameters. This SHALL hold whether the model is selected locally or by a mesh client.

#### Scenario: Selecting the profile's model keeps its parameters
- **WHEN** a profile sets `max_tokens = 16384` and `enable_thinking = false` for `llama_cpp` model `M`, and a client selects `llama_cpp/M` for a session
- **THEN** the session's stored configuration contains `max_tokens = 16384` and `enable_thinking = false`
- **AND** the provider for that session is built with those values

#### Scenario: Selecting another model does not inherit them
- **WHEN** the same profile is active and a client selects `llama_cpp/N`
- **THEN** the session's stored configuration contains none of the profile's parameters

#### Scenario: Switching back restores the profile's parameters
- **WHEN** a session selects `llama_cpp/N` and then selects `llama_cpp/M` again
- **THEN** the session's stored configuration contains the profile's parameters

### Requirement: Re-selecting the current model is a no-op
A model selection that names the provider, model and provider node the session is already bound to SHALL NOT change the session's stored configuration or emit model-change events. The local node SHALL be treated as one node whether it is given as no node, `local`, or this node's own id.

#### Scenario: Automatic re-selection after first use
- **WHEN** a session bound to the profile's model receives a selection for the same provider and model
- **THEN** its configuration binding is unchanged and no `ProviderChanged` event is emitted

#### Scenario: Re-selection keeps an explicit configuration
- **WHEN** a session's configuration was set with `set_llm_config` to `max_tokens = 4096` for the profile's model, and a client then selects that same model
- **THEN** the session keeps `max_tokens = 4096`

### Requirement: Explicit configuration takes precedence
A configuration set through `set_llm_config` SHALL be stored as given, without merging the profile's parameters. Parameters it omits SHALL fall back to built-in model defaults, not to the profile.

#### Scenario: Explicit configuration overrides the profile
- **WHEN** the profile sets `max_tokens = 16384` and `set_llm_config` stores `max_tokens = 4096` for the profile's model
- **THEN** the provider is built with `max_tokens = 4096`

#### Scenario: Profile overrides model defaults
- **WHEN** the profile sets `temperature = 0.6` for its model, the profile's model is selected, and the built-in defaults for that model suggest a different temperature
- **THEN** the provider is built with `temperature = 0.6`
