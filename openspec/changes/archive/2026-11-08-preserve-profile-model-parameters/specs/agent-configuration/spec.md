# Spec Delta

## ADDED Requirements

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
