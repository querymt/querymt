## Purpose

Define how QueryMT discovers, validates, layers, and applies portable `.agents/` configuration to agent runtimes while preserving existing QueryMT configuration and safe startup behavior.

## ADDED Requirements

### Requirement: Protocol support is explicitly enabled
The system SHALL expose configuration and programmatic API controls for enabling `.agents` Protocol loading. When protocol loading is disabled, or when no global or workspace protocol directory exists, the system SHALL preserve existing QueryMT behavior and SHALL NOT create a `.agents` directory.

#### Scenario: Protocol directory is absent
- **WHEN** protocol loading is enabled and neither configured layer exists
- **THEN** agent construction succeeds with the explicit QueryMT configuration unchanged
- **AND** no protocol directory or file is created

#### Scenario: Protocol support is disabled
- **WHEN** a `.agents` directory exists but protocol loading is disabled
- **THEN** the directory has no effect on the resolved agent runtime

### Requirement: Global and workspace layers are discovered deterministically
The system SHALL discover the global layer at `~/.agents/` and the workspace layer at `<workspace>/.agents/`, using the effective agent workspace as `<workspace>`. The system SHALL resolve layers in this order: explicit QueryMT base configuration, global protocol layer, workspace protocol layer. A caller SHALL be able to override or disable the global and workspace roots for deterministic embedding and tests.

#### Scenario: Both layers exist
- **WHEN** matching configuration is present in global and workspace layers
- **THEN** the workspace value wins over the global value
- **AND** the resolved manifest records the winning source

#### Scenario: Workspace is not configured
- **WHEN** the agent has no effective workspace
- **THEN** only the enabled global layer is considered
- **AND** the process working directory is not silently treated as a project unless the caller selected it as the workspace

### Requirement: Protocol files follow type-specific merge rules
The system SHALL treat `agents.md` and `system-prompt.md` as singleton documents, shallow-merge top-level keys in `mcp.json` and `models.json`, and merge skills, agents, tasks, and memories by stable entry ID. For singleton documents, the workspace document SHALL replace the global document. For keyed content, a workspace key or entry SHALL replace the matching global key or entry while unmatched global content remains.

#### Scenario: Workspace overrides one collection entry
- **WHEN** global and workspace layers define the same entry ID and the global layer contains another distinct ID
- **THEN** the workspace definition is resolved for the duplicate ID
- **AND** the distinct global definition remains available

#### Scenario: Workspace singleton is present
- **WHEN** both layers contain `system-prompt.md`
- **THEN** only the workspace `system-prompt.md` contributes protocol system-prompt content

### Requirement: Resolved configuration is inspectable
The system SHALL provide a typed resolved manifest containing supported documents, effective entries, source paths, source layers, and non-secret diagnostics before runtime side effects are reconciled. Ordering of entries and diagnostics SHALL be deterministic for identical files.

#### Scenario: Caller previews configuration
- **WHEN** a caller resolves a workspace containing valid protocol files
- **THEN** the caller can inspect effective instructions, MCP servers, model presets, skills, agents, tasks, memories, and diagnostics without starting scheduled work or ingesting memories

### Requirement: Markdown and JSON parsing is protocol compatible
The system SHALL parse protocol content files as frontmatter plus Markdown body and configuration files as JSON. Frontmatter SHALL accept the protocol's scalar values, quoted values, comma-separated list values, and JSON-array list values. Unknown frontmatter and JSON fields SHALL be retained or ignored for forward compatibility unless they conflict with a supported field.

#### Scenario: Minimal frontmatter is parsed
- **WHEN** a content document contains supported simple frontmatter and a Markdown body
- **THEN** its metadata and body are represented separately in the resolved manifest

#### Scenario: Future field is encountered
- **WHEN** a valid document includes an unknown field
- **THEN** the entry remains usable
- **AND** protocol loading does not fail solely because of that field

### Requirement: Invalid entries produce isolated diagnostics
The system SHALL report malformed files, duplicate IDs within one layer, missing required fields, unsupported values, and unavailable runtime mappings as diagnostics containing the source path and entry identity where available. An invalid entry SHALL NOT hide valid sibling entries. Errors that make a singleton file or the entire resolved runtime unsafe SHALL prevent applying that affected configuration rather than being silently ignored.

#### Scenario: One task is malformed
- **WHEN** a task directory contains one malformed task and one valid task
- **THEN** the valid task is present in the resolved manifest
- **AND** a diagnostic identifies the malformed task source

#### Scenario: Required singleton is invalid
- **WHEN** a selected singleton JSON file cannot be parsed
- **THEN** the system does not partially apply that file
- **AND** reports an actionable error for the selected source

### Requirement: Agent instructions augment the effective system prompt
The system SHALL read `.agents/agents.md` as AGENTS.md-compatible instructions, excluding protocol frontmatter from prompt content, and append the selected document body as a distinct system-prompt part after explicit QueryMT system-prompt parts. The same resolved instructions SHALL apply to single-agent roots and quorum agents unless an agent-specific profile overrides them.

#### Scenario: Instructions are available
- **WHEN** a workspace `agents.md` contains Markdown instructions
- **THEN** new sessions receive those instructions after the explicit QueryMT system prompt
- **AND** the frontmatter fence is not sent to the model

### Requirement: Protocol system prompt is composed predictably
The system SHALL append the selected `system-prompt.md` body after explicit QueryMT system-prompt parts and before `agents.md` instructions. Empty protocol prompt bodies SHALL have no effect.

#### Scenario: All prompt sources are present
- **WHEN** explicit QueryMT prompt parts, `system-prompt.md`, and `agents.md` are resolved
- **THEN** new sessions receive them in explicit, protocol-system-prompt, protocol-instructions order

### Requirement: MCP servers map to supported QueryMT transports
The system SHALL read `mcp.json` entries under `mcpServers` and support the protocol's `stdio` and `streamable-http` transports through QueryMT's existing MCP client lifecycle. It SHALL validate unique names and transport-specific fields, preserve command arguments, environment values, URLs, and supported headers, and expose WebSocket and unknown future transports as diagnostics until QueryMT can represent them.

#### Scenario: Stdio MCP server is resolved
- **WHEN** an enabled `mcpServers` entry defines a command, arguments, environment, and `transport: "stdio"`
- **THEN** the runtime starts it through QueryMT's existing stdio MCP transport
- **AND** exposes the server's tools through the existing MCP lifecycle

#### Scenario: Streamable HTTP MCP server is resolved
- **WHEN** an enabled entry defines a URL, supported headers, and `transport: "streamable-http"`
- **THEN** the runtime connects using QueryMT's existing streamable HTTP MCP transport
- **AND** exposes the server's tools through the existing MCP lifecycle

#### Scenario: Transport is inferred from a command
- **WHEN** an MCP entry omits `transport`, defines `command`, and does not define `url`
- **THEN** the system treats the entry as `stdio`

#### Scenario: Transport is inferred from a URL
- **WHEN** an MCP entry omits `transport`, defines `url`, and does not define `command`
- **THEN** the system treats the entry as `streamable-http`

#### Scenario: Transport fields conflict
- **WHEN** an MCP entry omits `transport` but defines both `command` and `url`
- **THEN** that server is not started
- **AND** a diagnostic requires an unambiguous supported transport configuration

#### Scenario: WebSocket transport is requested
- **WHEN** an MCP entry requests `websocket`
- **THEN** that server is not started
- **AND** a diagnostic identifies WebSocket as unsupported by the current QueryMT MCP runtime

#### Scenario: Future transport is unsupported
- **WHEN** an MCP entry requests an unknown transport QueryMT cannot represent
- **THEN** that server is not started
- **AND** a diagnostic identifies the server and transport

### Requirement: Model presets map to LLM configuration
The system SHALL parse named model presets from `models.json` into provider, model, credentials, and provider-parameter overlays that can be selected through the public API or referenced by a protocol agent or task. Selection SHALL fail with an actionable diagnostic when the preset, provider, or required model identifier cannot be resolved.

#### Scenario: Caller selects a model preset
- **WHEN** the caller selects a valid resolved preset
- **THEN** its provider, model, and supported parameters are applied to the runtime without dropping explicit unrelated QueryMT settings

#### Scenario: Referenced preset is unknown
- **WHEN** an enabled protocol agent or task references an unknown preset
- **THEN** that entry is not materialized
- **AND** a diagnostic names the missing preset and referring entry

### Requirement: Existing and protocol skills remain compatible
The system SHALL continue discovering existing configured, `.skills`, `.claude/skills`, `.qmt/skills`, and `.agents/skills` sources. Within `.agents/skills`, it SHALL accept protocol `skill.md` and existing `SKILL.md` naming, parse protocol `id`, `name`, `description`, and `enabled` metadata, and use the directory name as the ID when `id` is absent. Disabled skills SHALL not be exposed to the skill tool.

#### Scenario: Lowercase protocol skill is present
- **WHEN** `.agents/skills/review/skill.md` contains valid protocol metadata
- **THEN** it is discoverable with stable ID `review` unless an explicit ID is provided

#### Scenario: Existing uppercase skill is present
- **WHEN** an existing `.agents/skills/review/SKILL.md` is valid
- **THEN** it remains discoverable after protocol support is enabled

#### Scenario: Workspace skill overrides global skill
- **WHEN** global and workspace layers contain enabled skills with the same ID
- **THEN** only the workspace skill is exposed

### Requirement: Supported sub-agents become delegation targets
The system SHALL parse `.agents/agents/<entry>/agent.md` metadata and body, apply supported fields from adjacent `config.json`, and register each enabled, supported profile as a delegation target. The profile body SHALL become that target's agent-specific system prompt. Internal connections SHALL reuse QueryMT's local runtime, model, tool, MCP, and registry facilities. MCP transport support for a sub-agent is distinct from the sub-agent's own ACP or executable connection type.

#### Scenario: Internal sub-agent is valid
- **WHEN** an enabled profile has role `delegation-target` and a supported internal connection
- **THEN** it appears in agent discovery with its ID, name, description, and capabilities
- **AND** delegating to it starts or reuses the configured local runtime

#### Scenario: Profile is disabled
- **WHEN** a profile has `enabled: false`
- **THEN** it is not registered as a delegation target

#### Scenario: External connection cannot be represented safely
- **WHEN** a profile requests an unsupported executable or connection type
- **THEN** no process is launched
- **AND** a diagnostic identifies the unsupported connection

### Requirement: Repeat tasks reconcile with durable schedules
The system SHALL parse `.agents/tasks/<entry>/task.md` entries with `kind: task`, stable ID, prompt body, `intervalMinutes`, `enabled`, `runOnStartup`, and optional `profileId`. Enabled entries SHALL reconcile to one durable recurring task and interval schedule in the applicable QueryMT runtime. Reconciliation SHALL update changed protocol-owned records and SHALL NOT duplicate unchanged records across restarts.

#### Scenario: Enabled interval task is loaded
- **WHEN** a valid task specifies `intervalMinutes: 60`
- **THEN** one recurring task and interval schedule equivalent to 3600 seconds exist for its stable source identity

#### Scenario: Task is loaded again unchanged
- **WHEN** the same protocol task is reconciled after restart
- **THEN** no duplicate task or schedule is created

#### Scenario: Task is disabled or removed
- **WHEN** a previously reconciled protocol task becomes disabled or is removed
- **THEN** its protocol-owned schedule is paused or retired without deleting unrelated user-created schedules

#### Scenario: Startup execution is enabled
- **WHEN** an enabled task has `runOnStartup: true`
- **THEN** it is triggered once after successful reconciliation for that runtime startup
- **AND** normal interval scheduling remains active

### Requirement: Memories import idempotently into knowledge storage
The system SHALL parse `.agents/memories/*.md`, derive a stable source identity from the layer and memory ID, and import enabled memories into the configured knowledge store. It SHALL map the body and supported `content`, `title`, `tags`, and `importance` metadata to knowledge fields without creating duplicates on unchanged reloads. If no knowledge store is configured, memories SHALL remain inspectable and produce a diagnostic instead of failing unrelated protocol features.

#### Scenario: Memory is imported
- **WHEN** a valid memory is resolved and a knowledge store is available
- **THEN** one knowledge entry is stored with its text, summary, topics, importance, and stable protocol source

#### Scenario: Memory is reloaded unchanged
- **WHEN** the same memory is loaded again
- **THEN** a second knowledge entry is not created

#### Scenario: Knowledge storage is unavailable
- **WHEN** memories are present but the runtime has no knowledge store
- **THEN** agent startup can continue with other valid protocol features
- **AND** diagnostics report that memories were not imported

### Requirement: Filesystem resolution is confined and safe
The system SHALL reject path traversal outside a selected protocol entry root, unsafe symlink escapes, non-regular files where regular files are required, and relative executable or file references that escape their owning layer. Workspace discovery SHALL only occur when the workspace `.agents` directory already exists.

#### Scenario: Entry uses path traversal
- **WHEN** a protocol reference resolves outside its allowed root
- **THEN** the reference is rejected before any file is read or process is started
- **AND** a diagnostic identifies the unsafe source without exposing unrelated filesystem content

#### Scenario: Symlink escapes the layer
- **WHEN** a discovered entry resolves through a symlink outside the enabled protocol root
- **THEN** the entry is rejected unless the caller explicitly enabled that external root

### Requirement: Secrets are interpolated and redacted safely
The system SHALL support QueryMT-compatible environment interpolation for protocol credential, MCP environment, and header string values. Diagnostics, debug output, and inspectable manifests SHALL redact resolved secret values while retaining enough source and field context to correct errors. Secret values SHALL never be used as entry IDs or persisted as protocol reconciliation keys.

#### Scenario: Environment secret resolves
- **WHEN** a protocol field references an available environment variable
- **THEN** the runtime receives the resolved value
- **AND** manifest display and diagnostics do not expose the value

#### Scenario: Required environment secret is missing
- **WHEN** a required secret reference cannot be resolved
- **THEN** the affected server or preset is not activated
- **AND** the diagnostic names the variable reference but not any neighboring secrets

### Requirement: Protocol loading preserves existing QueryMT configuration
The system SHALL preserve current TOML loaders, programmatic builders, profile catalogs, skill sources, session model switching, and runtimes that do not opt into protocol loading. Protocol overlays SHALL modify only fields represented by present supported protocol files and SHALL NOT silently remove unrelated explicit settings.

#### Scenario: Existing TOML-only application starts
- **WHEN** an application uses the current QueryMT TOML configuration without enabling protocol loading
- **THEN** its prompts, tools, MCP servers, skills, profiles, and runtime behavior remain unchanged

#### Scenario: Partial protocol overlay is applied
- **WHEN** only `.agents/agents.md` exists
- **THEN** instructions are added to the prompt
- **AND** explicit model, MCP, tool, scheduler, and knowledge configuration remains unchanged
