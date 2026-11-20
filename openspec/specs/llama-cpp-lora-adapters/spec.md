# llama-cpp-lora-adapters Specification

## Purpose

Defines how the llama-cpp provider loads GGUF LoRA adapters together with a base model, applies them with a configurable strength to generation, rejects invalid or incompatible adapters, and how adapters affect reuse of a loaded model.

## Requirements

### Requirement: LoRA adapter option
The llama-cpp provider SHALL accept `lora` (in `[providers.config]` or `[agent.parameters]`): an array of tables with `path` (string, required) and `scale` (float, default `1.0`, any finite value, negative allowed). `path` SHALL accept a local GGUF path or a Hugging Face `<repo>:<selector>` ref, resolved like `model`. Omitting `lora` or setting `lora = []` SHALL load no adapter. Other keys in an entry SHALL be rejected.

#### Scenario: Lora omitted keeps the base model
- **WHEN** a provider config has no `lora` key
- **THEN** the config parses, no adapter is loaded and generation uses the base model only

#### Scenario: Adapter scale defaults to one
- **WHEN** `lora = [{ path = "/models/style.gguf" }]` is configured
- **THEN** the adapter is applied with scale `1.0`

#### Scenario: Unknown adapter key rejected
- **WHEN** an entry of `lora` contains a key other than `path` or `scale`, for example `weight = 0.5`
- **THEN** parsing the provider config fails with an error that names the unknown key

### Requirement: Adapter validation before loading
The provider SHALL validate every `lora` entry before the base model is loaded or downloaded. A non-finite `scale` SHALL fail with an error stating the scale must be a finite number. An invalid `path` SHALL fail with an error naming `lora[<i>].path` and the value. At most one adapter SHALL be accepted.

#### Scenario: Non-finite adapter scale rejected
- **WHEN** `lora = [{ path = "/models/style.gguf", scale = nan }]`, `scale = inf` or `scale = -inf` is configured
- **THEN** provider configuration fails with an error stating that the LoRA scale must be a finite number, and no model is loaded

#### Scenario: Missing adapter file rejected
- **WHEN** an entry's `path` is a local path that does not exist
- **THEN** provider configuration fails with an error naming `lora[0].path` and the path, and no model is loaded

#### Scenario: Hugging Face adapter without selector rejected
- **WHEN** an entry's `path` is a Hugging Face repo without a `:<selector>`
- **THEN** provider configuration fails with an error naming `lora[0].path` that asks for a selector

#### Scenario: More than one adapter rejected
- **WHEN** `lora` has two or more entries
- **THEN** provider configuration fails with an error naming `lora` that states only one adapter is supported

### Requirement: Adapter compatibility
An adapter that cannot be loaded onto the configured base model (a different architecture or tensor shapes, or a file that is not a LoRA adapter) SHALL fail provider configuration with an error naming `lora[<i>].path` and the adapter path. The process SHALL keep running.

#### Scenario: Incompatible adapter rejected
- **WHEN** `lora` points at a GGUF file that is not a LoRA adapter for the base model
- **THEN** provider configuration fails with an error naming the adapter path, and the provider can still be configured afterwards with a valid config

### Requirement: Adapter applied to generation
Every generation with a configured adapter SHALL apply it at its `scale`: chat, streaming chat, completion, tool-call generation, multimodal generation and the target model of speculative decoding. A `scale` of `0` SHALL produce the same output as the base model. The speculative draft model and the multimodal projector SHALL NOT receive the adapter.

#### Scenario: Adapter changes generated text
- **WHEN** the prompt `Look in thy glass` is completed greedily with the same seed on the `stories15M_MOE` base model once with its `moe_shakespeare15M` adapter at `scale = 1.0` and once without `lora`
- **THEN** the two outputs differ, and the adapted output contains Shakespearean vocabulary (`eye`, `love` or `sun`) that the base output does not

#### Scenario: Zero scale matches the base model
- **WHEN** the same prompt is completed greedily with the same seed once with an adapter at `scale = 0` and once without `lora`
- **THEN** the two outputs are identical

#### Scenario: Adapter applies to streaming output
- **WHEN** the prompt `Look in thy glass` is streamed greedily with the same seed on the `stories15M_MOE` base model once with its `moe_shakespeare15M` adapter at `scale = 1.0` and once with the same adapter at `scale = 0`
- **THEN** the two streamed texts differ, and the `scale = 0` text equals a streamed run without `lora`

#### Scenario: Adapter applies to tool-call, multimodal and speculative generation
- **WHEN** a request with an adapter configured runs through tool-call generation, multimodal generation or speculative decoding
- **THEN** the generation context of the target model has the adapter applied at its configured scale, and the draft model's context has no adapter

### Requirement: Adapters and model reuse
The ordered list of resolved adapter paths SHALL be part of a loaded model's identity: a provider configured with a different list SHALL load the model again. Adapter scales SHALL NOT be part of that identity: providers that differ only in scale SHALL reuse the loaded model and adapter, each applying its own scale.

#### Scenario: Changed adapter list reloads the model
- **WHEN** a model is loaded with `lora = [{ path = "/models/a.gguf" }]` and a provider is then configured with `lora = [{ path = "/models/b.gguf" }]` or with no `lora`
- **THEN** the second provider loads the model again instead of reusing the first

#### Scenario: Changed scale reuses the loaded model
- **WHEN** a model is loaded with `lora = [{ path = "/models/a.gguf", scale = 1.0 }]` and a provider is then configured with the same path and `scale = 0.5`
- **THEN** the second provider reuses the loaded model and adapter, and generates with scale `0.5`
