## 1. Protocol Domain and Parsing

- [ ] 1.1 Add the public `dotagents` module with load options, layer/source metadata, typed document models, redacted diagnostics, and the side-effect-free resolved manifest; verify public API compile tests can construct options and inspect every supported collection.
- [ ] 1.2 Implement global and workspace root discovery with explicit root overrides, disabled layers, deterministic path ordering, and no implicit directory creation; verify tests cover absent roots, no-workspace behavior, and both-layer discovery.
- [ ] 1.3 Implement confined file resolution that rejects traversal, non-regular files, and symlink escapes while allowing explicitly configured trusted roots; verify filesystem tests cover each rejection and do not read escaped content.
- [ ] 1.4 Implement shared frontmatter-plus-Markdown parsing with scalar, quoted, CSV-list, and JSON-array-list support plus extension fields; verify parser tests cover all protocol examples and malformed frontmatter diagnostics.
- [ ] 1.5 Implement parsers for `agents.md`, `system-prompt.md`, `mcp.json`, `models.json`, agent profiles/configs, repeat tasks, and memories; verify fixture tests parse minimal and full examples for each supported file.
- [ ] 1.6 Implement deterministic singleton replacement, JSON top-level key merging, collection ID merging, duplicate-within-layer errors, provenance, and content fingerprints; verify global/workspace precedence and stable ordering tests pass.
- [ ] 1.7 Implement strict and compatibility diagnostic policies so invalid siblings are isolated and unsafe selected singleton configuration blocks only its affected activation; verify mixed-validity fixture tests exercise both policies.

## 2. Prompt, MCP, Model, and Skill Adapters

- [ ] 2.1 Add protocol settings to TOML configuration and programmatic builders with protocol loading disabled by default; verify existing config/schema tests remain unchanged and new round-trip/default tests pass.
- [ ] 2.2 Compose explicit system parts, protocol `system-prompt.md`, and protocol `agents.md` in the specified order for single-agent and quorum runtimes; verify session persistence, model switching, and remote prompt forwarding retain the composed prompt without frontmatter.
- [ ] 2.3 Convert protocol `stdio` and `streamable-http` MCP entries into the existing process and RMCP streamable HTTP configurations, preserving arguments, environment, URLs, and headers; verify conversion tests use the existing transports rather than a protocol-specific client stack.
- [ ] 2.4 Infer omitted MCP transport only for unambiguous `command`-only and `url`-only entries, and reject conflicting fields, WebSocket, and unknown transports with source-aware diagnostics; verify every support-matrix case has a parser test.
- [ ] 2.5 Add secret-aware environment interpolation and redacted public/debug representations for MCP and model values; verify missing-variable errors are actionable and serialization/debug tests cannot find resolved secret literals.
- [ ] 2.6 Convert named `models.json` presets into selectable LLM overlays without installing providers or dropping unrelated base parameters; verify valid selection, unknown preset, missing provider/model, and unavailable provider tests pass.
- [ ] 2.7 Extend skill discovery to accept protocol `skill.md`, existing `SKILL.md`, protocol IDs and enabled state, while preserving all current skill sources and precedence; verify lowercase, uppercase, duplicate-case, disabled, recursive, and regression tests pass.
- [ ] 2.8 Expose manifest preview and resolved diagnostics through the public API without starting MCP servers or running persistence reconciliation; verify a preview integration test observes all effective sections and zero side effects.

## 3. Internal Sub-Agent Integration

- [ ] 3.1 Map protocol agent metadata and supported `config.json` tool/model/MCP settings into a neutral sub-agent runtime plan; verify disabled profiles, unknown presets, unsupported fields, and unsupported connection diagnostics are covered.
- [ ] 3.2 Implement lazy materialization for enabled internal `delegation-target` profiles using shared agent infrastructure and stable IDs; verify registered `AgentInfo` metadata and delegation to the local handle work in single-agent and quorum integration tests.
- [ ] 3.3 Add inheritance and recursion guards so child protocol agents receive intended singleton/base settings without recursively registering agents or reconciling tasks; verify nested materialization creates one handle per target and terminates deterministically.
- [ ] 3.4 Reject stdio executable and unknown sub-agent connection types without launching a process; verify a test command marker is never created and diagnostics identify the profile and connection type.

## 4. Repeat Task Reconciliation

- [ ] 4.1 Define stable protocol ownership and creation keys for tasks and schedules, and add additive repository lookup/upsert support as needed; verify storage migration and repository tests distinguish protocol-owned records from user-created records.
- [ ] 4.2 Implement conversion from enabled protocol task metadata/body to recurring tasks and checked interval schedules bound to the designated automation session/profile; verify interval-minute conversion, overflow rejection, prompt mapping, and profile resolution tests pass.
- [ ] 4.3 Implement idempotent create/update reconciliation based on source identity and content fingerprint; verify repeated unchanged startup creates one task/schedule and changed content updates the protocol-owned records.
- [ ] 4.4 Pause or retire removed and disabled protocol schedules without modifying unrelated schedules; verify reconciliation tests preserve user-created records and handle global entries overridden by workspace entries.
- [ ] 4.5 Implement `runOnStartup` once per runtime startup after successful reconciliation while retaining normal interval activation; verify restart simulations trigger exactly once per startup and do not duplicate durable records.
- [ ] 4.6 Return non-fatal diagnostics when schedule storage, target profile, or scheduler activation is unavailable in compatibility mode and fail appropriately in strict mode; verify both activation policies with integration tests.

## 5. Memory Reconciliation

- [ ] 5.1 Define stable protocol memory source keys and extend the knowledge abstraction/storage with narrowly scoped upsert or source reconciliation semantics for changed and removed protocol-owned memories; verify additive migration and source lifecycle repository tests pass.
- [ ] 5.2 Map memory body/content, title, tags, and protocol importance levels into knowledge fields with deterministic summaries and normalized scores; verify mapping tests cover body fallback, metadata content, tags, and each accepted importance form.
- [ ] 5.3 Implement idempotent memory import and update reconciliation using source identity and fingerprints; verify unchanged reloads do not duplicate entries and edits replace or deactivate stale protocol-owned content.
- [ ] 5.4 Preserve inspectable memories and emit non-fatal diagnostics when no knowledge store is configured, without blocking other protocol features; verify an agent starts with prompts/skills active and reports skipped memory import.

## 6. Runtime Wiring and Compatibility

- [ ] 6.1 Apply resolved overlays before final single-agent `AgentConfig` construction while preserving unrelated explicit settings; verify partial-overlay tests retain explicit model, tools, middleware, hooks, scheduler, and knowledge configuration.
- [ ] 6.2 Apply resolved overlays to quorum planner and delegates and close the simple quorum builder's MCP attachment gap for existing stdio and streamable HTTP configurations; verify planner and delegate integration tests can invoke tools from each supported transport.
- [ ] 6.3 Thread protocol workspace and selected preset context through profile runtime construction and session binding without making the TOML profile catalog parse protocol profiles; verify profile reload and bound-session behavior remain stable.
- [ ] 6.4 Add end-to-end fixtures containing both global and workspace protocol trees and verify precedence, prompts, MCP plans, model presets, skills, internal agents, tasks, memories, provenance, and diagnostics in one runtime test.
- [ ] 6.5 Run existing targeted regression suites for config loading, skills, profiles, delegation, schedules, knowledge, model switching, and remote system prompts; verify all previously supported TOML-only and programmatic paths pass with protocol loading disabled.

## 7. Documentation and Quality Gates

- [ ] 7.1 Document opt-in configuration, programmatic loading/preview, supported directory layout, exact precedence, security rules, and protocol-to-QueryMT mappings in the existing agent documentation; verify examples match the generated configuration schema.
- [ ] 7.2 Document unsupported `speakmcp-settings.json`, `layouts/`, `.backups/`, Hub/bundle operations, write-back, external sub-agent execution, and WebSocket MCP transport while explicitly listing MCP stdio and streamable HTTP as supported; verify the support matrix covers every top-level protocol entry and MCP transport case.
- [ ] 7.3 Record the dependency evaluation showing that `dotagents` is a binary-only tool for a different `.dotagents/` convention and `agent-runbooks` only initializes unrelated runbooks, and verify neither crate is added to `Cargo.lock`.
- [ ] 7.4 Run `cargo fmt --all -- --check` and fix formatting until it succeeds.
- [ ] 7.5 Run `cargo test -p querymt-agent` and fix all failures until the crate test suite succeeds.
- [ ] 7.6 Run `cargo clippy -p querymt-agent --all-targets --all-features -- -D warnings` and resolve all introduced warnings, documenting any pre-existing blocker with its exact command output.
- [ ] 7.7 Run `openspec validate add-dotagents-protocol-support --strict` and resolve all proposal, design, specification, and task validation errors.
