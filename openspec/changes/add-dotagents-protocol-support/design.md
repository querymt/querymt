## Context

See `proposal.md` for motivation and `specs/dotagents-protocol/spec.md` for the behavior contract.

`querymt-agent` already has most target runtime concepts, but they enter through separate paths:

- Skills discover global and project sources and already include `~/.agents/skills` and `<project>/.agents/skills`.
- Single-agent and quorum TOML configurations resolve system prompt parts and MCP server definitions into runtime builders.
- Local TOML profile catalogs lazily build single-agent or quorum runtimes and maintain session-to-profile bindings.
- The delegation registry can expose local agent handles as delegation targets.
- Durable recurring tasks and interval schedules are persisted and managed by the scheduler.
- Knowledge stores support idempotency checks based on source keys.

The protocol is a draft and its website describes conventions rather than a complete versioned schema. The implementation therefore needs an explicit support boundary, forward-compatible parsing, source-aware diagnostics, and adapters that do not make protocol parsing depend directly on runtime construction.

## Goals / Non-Goals

**Goals:**

- Represent the supported protocol as typed documents and a deterministic resolved manifest.
- Keep discovery and parsing side-effect free; make task and memory reconciliation an explicit later phase.
- Reuse current QueryMT configuration and runtime facilities rather than creating parallel MCP, model, delegation, scheduler, or knowledge systems.
- Preserve provenance through layering so errors and UI/API inspection identify the effective source.
- Permit embedders to override roots, select model presets, inspect diagnostics, and decide whether errors prevent activation.
- Keep protocol support additive and disabled/configurable so current applications do not change behavior unexpectedly.

**Non-Goals:**

- Implement `.dotagents` bundle creation, Hub publishing/installing, backup rotation, or write-back to protocol files.
- Interpret `speakmcp-settings.json`, `layouts/`, or `.backups/`.
- Implement arbitrary external sub-agent executables until there is a sandboxed client-side ACP process lifecycle in the agent crate.
- Replace QueryMT TOML profiles or provider/plugin provisioning with `models.json`.
- Guarantee compatibility with undocumented fields beyond retaining or diagnosing them safely.

## Decisions

### 1. Add a dedicated protocol boundary

Create a public `dotagents` module organized around:

- `DotagentsLoadOptions`: enable flag, workspace, global/workspace root overrides, selected model preset, and strictness.
- Parsed document types for singleton Markdown, MCP/model JSON, skills, agents, tasks, and memories.
- `DotagentsManifest`: fully layered effective values, source metadata, and diagnostics.
- `DotagentsLoader`: discovery, parsing, validation, and merge without runtime side effects.
- `DotagentsRuntimeOverlay`: conversion of a manifest into prompt, LLM, MCP, skill, and sub-agent runtime inputs.
- `DotagentsReconciler`: explicit asynchronous task and memory persistence after the runtime and storage services exist.

This split makes manifest preview safe and allows the same parser to serve direct builders, profile runtimes, tests, and future UI support.

Alternative considered: extend `config::load_config` to read `.agents` directly. Rejected because the TOML loader resolves one document into `Config`, while protocol loading combines multiple roots and includes persistent side effects that do not belong in TOML deserialization.

### 2. Preserve explicit QueryMT config as the base

Apply values in this order:

1. Explicit QueryMT config or programmatic builder state.
2. Global protocol layer.
3. Workspace protocol layer.

Within a protocol layer, each singleton is one complete document; JSON objects merge by top-level key; collection entries merge by normalized ID. Protocol files only affect represented fields. Prompt documents are intentionally additive to the explicit prompt in the order specified by the capability spec.

Alternative considered: make explicit QueryMT config always highest priority. Rejected because it prevents workspace protocol files from acting as the documented override layer. Explicit callers that require isolation can disable protocol loading or select custom roots.

### 3. Use provenance-bearing values and deterministic diagnostics

Every effective document or entry carries:

- normalized ID or singleton kind;
- global/workspace layer;
- lexical and canonical source paths;
- optional content fingerprint;
- diagnostics attached during parse, merge, adaptation, or reconciliation.

Sort directory entries, IDs, and diagnostics before returning a manifest. Within one layer, duplicate normalized IDs are errors rather than filesystem-order-dependent winners. Cross-layer duplicates follow workspace precedence.

Alternative considered: log parse failures and return only successful values. Rejected because embedders and users need machine-readable errors, and logging alone can expose secrets or make partial startup impossible to reason about.

### 4. Centralize constrained frontmatter parsing

Build a shared protocol frontmatter parser over the existing Markdown/frontmatter dependency. Normalize the protocol's CSV and JSON-array list forms before deserializing typed metadata. Store unknown fields in extension maps. Strip frontmatter from runtime prompt/body content.

Skill parsing remains compatible with its existing richer metadata. The skill adapter accepts both `skill.md` and `SKILL.md`; if both exist in one entry directory, emit a duplicate-definition diagnostic instead of choosing by platform-specific ordering.

Alternative considered: reuse each existing content parser unchanged. Rejected because the protocol's simple list syntax, `id`, `enabled`, source diagnostics, and lowercase filenames need consistent behavior across content types.

### 5. Resolve paths through an explicit confinement policy

For each enabled layer, canonicalize the layer root once. Discovery reads only expected singleton paths and direct protocol collection entries. Before reading a file or accepting a referenced path:

- verify it is a regular file;
- canonicalize it;
- verify the canonical target remains under the allowed root;
- reject `..` escapes and symlink escapes;
- never create a workspace `.agents` directory during discovery.

Explicit additional roots may be allowed by load options and are treated as separate trusted roots rather than exceptions to confinement.

Alternative considered: permit symlinks because developer configuration is local. Rejected because workspace repositories can contain untrusted symlinks and protocol entries can carry commands and credentials.

### 6. Adapt prompt files at config-to-runtime boundaries

Resolve protocol singleton bodies into distinct system parts. Extend single-agent and quorum builder conversion so the final order is:

1. explicit system parts;
2. selected protocol `system-prompt.md` body;
3. selected protocol `agents.md` body;
4. existing later runtime additions such as delegated planning context.

Persist the composed system array in normal session LLM configuration so model switching and remote forwarding retain it through existing mechanisms.

Alternative considered: inject instructions at each prompt call. Rejected because it would duplicate content, diverge between local and remote sessions, and bypass existing system-prompt persistence.

### 7. Convert MCP definitions through existing configuration and transports

Parse `mcpServers` into a neutral protocol type first, then convert `stdio` to the existing process-based MCP configuration and `streamable-http` to the existing HTTP configuration backed by RMCP's streamable HTTP client. These transports are fully in scope; this change does not add a parallel MCP client stack.

Normalize omitted transport from structural fields only when unambiguous: `command` without `url` implies `stdio`, and `url` without `command` implies `streamable-http`. Conflicting fields are errors. WebSocket and unknown future MCP transports remain manifest diagnostics and are not started until the QueryMT MCP configuration and runtime can represent them.

Interpolate string values using the current QueryMT environment-reference rules during activation, not when serializing or displaying the manifest. Wrap secret-bearing values in redacting debug/display types or expose only redacted views publicly.

MCP transport conversion and MCP attachment are separate concerns. Single-agent runtimes already attach stdio and streamable HTTP servers. The simple quorum builder currently discards resolved planner/delegate MCP servers with warnings; implementation SHALL close that attachment gap for supported transports rather than treating them as unsupported protocol transports.

Alternative considered: deserialize JSON directly into the existing enum. Rejected because protocol field names and transport spellings differ and direct deserialization would produce poor source diagnostics.

Alternative considered: add a protocol-specific HTTP/WebSocket client. Rejected because stdio and streamable HTTP already exist in QueryMT, while WebSocket should be added first to the common MCP runtime if it becomes supported.

### 8. Treat model presets as overlays, not provider installation

Define protocol model presets as a name plus provider, model, optional credential reference, and provider parameters. A selected preset overlays corresponding LLM fields while preserving unrelated base settings. Agent and task references resolve against the effective preset map.

Provider availability is validated against the injected plugin registry during runtime adaptation. `models.json` does not install providers or replace profile provider locks. Plain credential strings can be consumed for compatibility but are redacted; environment references remain recommended.

Alternative considered: convert every preset into a QueryMT profile. Rejected because profiles include substantially more runtime and provider-lock semantics, while the protocol describes model presets rather than complete QueryMT runtimes.

### 9. Extend, but do not implicitly enable, delegation

Map an enabled `delegation-target` profile with `connection-type: internal` to a lazily constructed local agent handle. Compose the profile Markdown body as its specific system prompt and apply supported model preset, tools, and MCP restrictions from `config.json`.

For a standalone agent with delegation enabled, register protocol `AgentInfo` and handles in its existing delegation registry. For a quorum profile with delegation enabled, add protocol targets as delegates alongside the explicitly configured quorum delegates; do not replace the planner or reinterpret the profile as a different runtime type. Protocol loading never changes a disabled delegation setting to enabled.

Resolve collisions in two stages. Global and workspace protocol profiles first merge by ID using normal workspace precedence. The resulting protocol targets are then merged with explicit QueryMT registry targets or quorum delegates, where explicit QueryMT configuration wins. Skip the colliding protocol target and emit a provenance-bearing diagnostic rather than silently replacing either runtime.

Use shared infrastructure and storage in the same manner as profile runtimes, while ensuring each protocol agent has a stable ID and does not recursively rediscover and register itself. Introduce a load-context marker or disable collection materialization in child overlays to prevent recursion.

For stdio/executable or unknown connection types, retain metadata and emit an unsupported diagnostic; do not launch a process. ACP server support currently exposes QueryMT as a server and is not a safe client process manager.

Alternative considered: always let workspace protocol targets replace configured quorum delegates. Rejected because a repository overlay should not silently replace an explicitly selected runtime topology or trusted delegate implementation.

Alternative considered: launch `connection.command` directly. Rejected because the crate lacks the required permission, sandbox, cancellation, authentication, and ACP client lifecycle contract.

### 10. Gate workspace task reconciliation on explicit trust

Task documents describe durable schedules but existing schedules require a session. Parsing and manifest preview remain side-effect free. Reconciliation runs only after the selected profile runtime, designated persistent automation session, and task trust policy are available. Derive deterministic creation/source keys from protocol layer identity, task ID, and profile binding. Convert `intervalMinutes` with checked multiplication to interval seconds.

Treat workspace task files as untrusted repository content. Before persistence or execution, emit a structured approval request through a host-provided trust interface containing the canonical workspace, task ID/name, prompt summary, interval, `runOnStartup`, target profile, and source. Store approvals against a canonical workspace identity, normalized task ID, and fingerprint over all execution-relevant effective fields. A changed fingerprint invalidates approval and pauses any existing protocol-owned schedule before it can run again.

Support explicit task trust policies suitable for different hosts:

- `prompt` (default): request approval and keep the task pending when no approval mechanism exists;
- `deny`: never activate workspace protocol tasks;
- `allow` (unsafe): activate without prompting and emit a prominent diagnostic.

The agent crate exposes the policy and approval request/response boundary; CLI, ACP, UI, and embedding hosts decide how to present the confirmation. Global tasks may use a separately configured policy because `~/.agents` is user-controlled, but no implicit policy may weaken workspace defaults.

For each trusted enabled task:

- ensure the target profile/model reference resolves;
- ensure one recurring task and schedule exist;
- update protocol-owned prompt, interval, and enabled state when the fingerprint changes and renewed trust exists;
- fire once per runtime startup after successful reconciliation when `runOnStartup` is true.

For removed, disabled, changed-but-unapproved, or trust-revoked entries, pause or retire only records carrying the matching protocol ownership key. Never mutate user-created schedules. Persistence requires repository methods for lookup/upsert by protocol source key and trust state rather than implementing idempotency in memory.

Alternative considered: trust any task merely because protocol loading is enabled. Rejected because a cloned repository could silently schedule tool-capable prompts or execute them at startup.

Alternative considered: create a fresh session and schedule on every startup. Rejected because it duplicates durable records and defeats portable declarative configuration.

### 11. Import memories through source-key idempotency

Map memory body/content to raw text, title or a concise deterministic fallback to summary, tags to topics, and protocol importance values to the knowledge store's normalized score. Use a source key containing protocol namespace, layer identity, and memory ID, plus a content fingerprint for change detection.

Use existing source-ingestion checks for unchanged content. To represent updates and removals faithfully, extend the knowledge abstraction with protocol-owned upsert/deactivate semantics or an equivalent source reconciliation API; do not append a new active entry on every edit. If storage is absent, return diagnostics and continue activating unrelated features.

Alternative considered: inject memory files directly into every system prompt. Rejected because the crate already has scoped, searchable knowledge storage and prompt injection would scale poorly.

### 12. Do not depend on similarly named but incompatible crates

The `dotagents` and `agent-runbooks` crates were evaluated as potential helpers. Neither is an implementation of the protocol targeted by this change:

- `dotagents` 0.1.x is a binary-only `.dotagents/` configuration deployment and Handlebars templating tool. It has no library target and uses a different directory, schema, and write-oriented workflow.
- `agent-runbooks` 0.1.x exposes only initialization of bundled files under `.agent/runbooks/`. It does not parse or model `.agents` Protocol instructions, MCP, models, skills, agents, tasks, or memories.

Do not add either dependency. Reuse QueryMT's existing serde/JSON, frontmatter, RMCP, skill, profile, delegation, scheduler, and knowledge components. Keep the protocol boundary sufficiently isolated that a future authoritative protocol library could replace parsing without changing runtime adapters.

Alternative considered: invoke the `dotagents` executable or copy its internal modules. Rejected because that introduces a distinct configuration standard, write side effects, CLI coupling, and a large dependency surface without reusable protocol semantics.

Alternative considered: adopt `agent-runbooks` and extend the protocol scope with runbooks. Rejected because runbooks are not part of the current `.agents` Protocol and the crate provides no relevant parser.

### 13. Integrate through explicit builder and profile entry points

Add builder methods to enable protocol loading, provide load options, or supply a pre-resolved manifest. TOML configuration receives an optional protocol settings section with defaults that preserve current behavior. Apply the overlay before final `AgentConfig` construction so tools, prompts, MCP, and registry state use normal runtime paths.

Extend profile runtime construction to accept a protocol context for its workspace and selected preset. Do not make the generic local TOML profile catalog scan `.agents` as TOML profiles; protocol sub-agents use a dedicated adapter because their schema and merge behavior differ.

A strict activation mode fails construction on error diagnostics. The default compatibility mode activates unaffected valid entries while returning/logging structured warnings and fails only when the selected root configuration cannot be applied safely.

Alternative considered: automatically enable protocol loading for every builder. Rejected to avoid filesystem-dependent behavior changes for existing embedders.

## Risks / Trade-offs

- [The upstream protocol is a draft and can change] -> Keep protocol types isolated, preserve extensions, publish a support matrix, and test accepted aliases explicitly.
- [Layer precedence can surprise callers with existing explicit config] -> Require opt-in, expose resolved provenance, and allow roots/layers to be disabled independently.
- [Startup gains filesystem and persistence work] -> Keep parse/resolve synchronous and bounded, materialize sub-agents lazily, batch reconciliation, and avoid recursive scans except existing skill compatibility paths.
- [Task reconciliation can corrupt user schedules] -> Require protocol ownership keys and repository-level compare/update operations; never infer ownership from names alone.
- [A repository can smuggle malicious scheduled or startup prompts] -> Default workspace tasks to fingerprint-bound approval, keep them inactive in headless mode without approval, pause them on changes or trust revocation, and make unsafe allow an explicit auditable policy.
- [Memory edits and removals exceed current append-oriented APIs] -> Add narrowly scoped source reconciliation semantics and migration tests before enabling automatic imports.
- [Secrets in inspectable configuration can leak] -> Separate internal secret values from public/redacted manifest views and add debug/serialization leak tests.
- [Protocol sub-agents can recursively load collections] -> Carry a materialization depth/context flag and disable inherited agent/task reconciliation for child construction.
- [Protocol targets can collide with trusted standalone or quorum delegates] -> Merge protocol layers first, then give explicit QueryMT targets precedence and emit source-aware collision diagnostics.
- [The simple quorum API currently drops resolved MCP servers] -> Wire supported stdio and streamable HTTP MCP configurations into planner and delegate handles using the existing lifecycle, with integration tests proving attachment; do not misclassify this wiring gap as a transport limitation.
- [Global configuration is user-controlled while workspace configuration may be repository-controlled] -> Apply the same path confinement and command safety rules to both; do not activate unsupported executable agents.

## Migration Plan

1. Add parser, manifest, diagnostics, and preview APIs behind disabled-by-default protocol settings.
2. Add prompt, MCP, model, and skill adapters with compatibility tests; no persistence migration is needed.
3. Add internal sub-agent materialization with recursion guards and shared-infrastructure tests.
4. Add storage schema/repository support for protocol-owned task and memory reconciliation, including migrations that are additive and backward compatible.
5. Enable task and memory reconciliation only when their backing service is present; otherwise retain diagnostics.
6. Document supported files, precedence, security behavior, and opt-in examples.
7. Roll back by disabling protocol loading. Existing TOML and programmatic configuration remains usable, and protocol-owned durable records are paused/ignored rather than destructively deleting user data.
