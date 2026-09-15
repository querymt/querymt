## 1. Protocol Domain and Parsing

- [x] 1.1 Add the public `dotagents` module with load options, layer/source metadata, typed document models, redacted diagnostics, and the side-effect-free resolved manifest; verify public API compile tests can construct options and inspect every supported collection.
- [x] 1.2 Implement global and workspace root discovery with explicit root overrides, disabled layers, deterministic path ordering, and no implicit directory creation; verify tests cover absent roots, no-workspace behavior, and both-layer discovery.
- [x] 1.3 Implement confined file resolution that rejects traversal, non-regular files, and symlink escapes while allowing explicitly configured trusted roots; verify filesystem tests cover each rejection and do not read escaped content.
- [x] 1.4 Implement shared frontmatter-plus-Markdown parsing with scalar, quoted, CSV-list, and JSON-array-list support plus extension fields; verify parser tests cover all protocol examples and malformed frontmatter diagnostics.
- [x] 1.5 Implement parsers for `agents.md`, `system-prompt.md`, `mcp.json`, `models.json`, agent profiles/configs, repeat tasks, and memories; verify fixture tests parse minimal and full examples for each supported file.
- [x] 1.6 Implement deterministic singleton replacement, JSON top-level key merging, collection ID merging, duplicate-within-layer errors, provenance, and content fingerprints; verify global/workspace precedence and stable ordering tests pass.
- [x] 1.7 Implement strict and compatibility diagnostic policies so invalid siblings are isolated and unsafe selected singleton configuration blocks only its affected activation; verify mixed-validity fixture tests exercise both policies.

## 2. Prompt, MCP, Model, and Skill Adapters

- [x] 2.1 Add protocol settings to TOML configuration and programmatic builders with protocol loading disabled by default; verify existing config/schema tests remain unchanged and new round-trip/default tests pass.
- [x] 2.2 Compose explicit system parts, protocol `system-prompt.md`, and protocol `agents.md` in the specified order for single-agent and quorum runtimes; verify session persistence, model switching, and remote prompt forwarding retain the composed prompt without frontmatter.
- [x] 2.3 Convert protocol `stdio` and `streamable-http` MCP entries into the existing process and RMCP streamable HTTP configurations, preserving arguments, environment, URLs, and headers; verify conversion tests use the existing transports rather than a protocol-specific client stack.
- [x] 2.4 Infer omitted MCP transport only for unambiguous `command`-only and `url`-only entries, and reject conflicting fields, WebSocket, and unknown transports with source-aware diagnostics; verify every support-matrix case has a parser test.
- [x] 2.5 Add secret-aware environment interpolation and redacted public/debug representations for MCP and model values; verify missing-variable errors are actionable and serialization/debug tests cannot find resolved secret literals.
- [ ] 2.6 Convert named `models.json` presets into selectable LLM overlays without installing providers or dropping unrelated base parameters; verify valid selection, unknown preset, missing provider/model, and unavailable provider tests pass.
- [x] 2.7 Extend skill discovery to accept protocol `skill.md`, existing `SKILL.md`, protocol IDs and enabled state, while preserving all current skill sources and precedence; verify lowercase, uppercase, duplicate-case, disabled, recursive, and regression tests pass.
- [x] 2.8 Expose manifest preview and resolved diagnostics through the public API without starting MCP servers or running persistence reconciliation; verify a preview integration test observes all effective sections and zero side effects.

## 3. Internal Sub-Agent Integration

- [x] 3.1 Map protocol agent metadata and supported `config.json` tool/model/MCP settings into a neutral sub-agent runtime plan; verify disabled profiles, unknown presets, unsupported fields, and unsupported connection diagnostics are covered.
- [x] 3.2 Extend delegation-enabled standalone agent registries with lazy internal protocol targets, without enabling delegation when it is disabled; verify registry discovery and delegation work and disabled runtimes remain unchanged.
- [x] 3.3 Extend delegation-enabled quorum profiles with protocol targets as additional delegates without replacing the planner or configured delegates; verify configured quorum topology and delegate behavior remain intact.
- [x] 3.4 Implement collision resolution that first applies global/workspace protocol precedence and then gives explicit standalone registry targets and quorum delegates precedence over matching protocol IDs; verify skipped protocol targets produce diagnostics containing both sources.
- [x] 3.5 Add inheritance and recursion guards so child protocol agents receive intended singleton/base settings without recursively registering agents or reconciling tasks; verify nested materialization creates one handle per target and terminates deterministically.
- [x] 3.6 Reject stdio executable and unknown sub-agent connection types without launching a process; verify a test command marker is never created and diagnostics identify the profile and connection type.

## 4. Repeat Task Reconciliation

- [x] 4.1 Define stable protocol ownership and creation keys for tasks and schedules, plus persisted trust keyed by canonical workspace, normalized task ID, and execution-relevant fingerprint; verify storage migrations distinguish protocol-owned records and approvals from user-created schedules.
- [x] 4.2 Add host-facing task trust policy and approval request/response APIs with `prompt` as the workspace default, `deny`, and explicitly unsafe `allow`; verify headless `prompt` remains pending and `allow` emits a prominent diagnostic.
- [x] 4.3 Ensure parsing and preview never persist, schedule, or execute tasks, and disclose source, schedule, startup behavior, target profile, and prompt summary in approval requests; verify zero-side-effect and disclosure tests pass.
- [ ] 4.4 Implement conversion from trusted enabled protocol task metadata/body to recurring tasks and checked interval schedules bound to the designated automation session/profile; verify interval-minute conversion, overflow rejection, prompt mapping, and profile resolution tests pass.
- [ ] 4.5 Implement idempotent create/update reconciliation based on source identity and content fingerprint; verify repeated unchanged startup creates one task/schedule and execution-relevant changes require renewed approval before update or execution.
- [ ] 4.6 Pause or retire removed, disabled, changed-but-unapproved, and trust-revoked protocol schedules without modifying unrelated schedules; verify reconciliation preserves user-created records and handles global entries overridden by workspace entries.
- [ ] 4.7 Implement `runOnStartup` once per runtime startup only after successful reconciliation and trust approval while retaining normal interval activation; verify untrusted startup tasks never run and trusted restart simulations trigger exactly once per startup.
- [ ] 4.8 Return non-fatal diagnostics when trust, schedule storage, target profile, or scheduler activation is unavailable in compatibility mode and fail appropriately in strict mode; verify each activation policy with integration tests.

## 5. Memory Reconciliation

- [ ] 5.1 Define stable protocol memory source keys and extend the knowledge abstraction/storage with narrowly scoped upsert or source reconciliation semantics for changed and removed protocol-owned memories; verify additive migration and source lifecycle repository tests pass.
- [x] 5.2 Map memory body/content, title, tags, and protocol importance levels into knowledge fields with deterministic summaries and normalized scores; verify mapping tests cover body fallback, metadata content, tags, and each accepted importance form.
- [ ] 5.3 Implement idempotent memory import and update reconciliation using source identity and fingerprints; verify unchanged reloads do not duplicate entries and edits replace or deactivate stale protocol-owned content.
- [ ] 5.4 Preserve inspectable memories and emit non-fatal diagnostics when no knowledge store is configured, without blocking other protocol features; verify an agent starts with prompts/skills active and reports skipped memory import.

## 6. Runtime Wiring and Compatibility

- [ ] 6.1 Apply resolved overlays before final single-agent `AgentConfig` construction while preserving unrelated explicit settings; verify partial-overlay tests retain explicit model, tools, middleware, hooks, scheduler, and knowledge configuration.
- [ ] 6.2 Apply resolved overlays to quorum planner and delegates and close the simple quorum builder's MCP attachment gap for existing stdio and streamable HTTP configurations; verify planner and delegate integration tests can invoke tools from each supported transport.
- [x] 6.3 Thread protocol workspace and selected preset context through profile runtime construction and session binding without making the TOML profile catalog parse protocol profiles; verify profile reload and bound-session behavior remain stable.
- [ ] 6.4 Add end-to-end fixtures containing both global and workspace protocol trees and verify precedence, prompts, MCP plans, model presets, skills, standalone/quorum delegate collisions, task approval and revocation, memories, provenance, and diagnostics in runtime tests.
- [x] 6.5 Run existing targeted regression suites for config loading, skills, profiles, delegation, schedules, knowledge, model switching, and remote system prompts; verify all previously supported TOML-only and programmatic paths pass with protocol loading disabled.

## 7. Documentation and Quality Gates

- [x] 7.1 Document opt-in configuration, programmatic loading/preview, supported directory layout, exact precedence, standalone/quorum sub-agent extension and collision rules, workspace-task approval policies, security rules, and protocol-to-QueryMT mappings in the existing agent documentation; verify examples match the generated configuration schema.
- [x] 7.2 Document unsupported `speakmcp-settings.json`, `layouts/`, `.backups/`, Hub/bundle operations, write-back, external sub-agent execution, and WebSocket MCP transport while explicitly listing MCP stdio and streamable HTTP as supported; verify the support matrix covers every top-level protocol entry and MCP transport case.
- [x] 7.3 Record the dependency evaluation showing that `dotagents` is a binary-only tool for a different `.dotagents/` convention and `agent-runbooks` only initializes unrelated runbooks, and verify neither crate is added to `Cargo.lock`.
- [x] 7.4 Run `cargo fmt --all -- --check` and fix formatting until it succeeds.
- [x] 7.5 Run `cargo test -p querymt-agent` and fix all failures until the crate test suite succeeds.
- [x] 7.6 Run `cargo clippy -p querymt-agent --all-targets --all-features -- -D warnings` and resolve all introduced warnings, documenting any pre-existing blocker with its exact command output.
- [x] 7.7 Run `openspec validate add-dotagents-protocol-support --strict` and resolve all proposal, design, specification, and task validation errors.

## 8. Review Remediation

- [ ] 8.1 Wire task reconciliation into standalone and quorum runtime startup after storage, trust policy, automation session, and profiles are available; verify create, update, retirement, approval, and `runOnStartup` through runtime-level integration tests.
- [ ] 8.2 Wire memory reconciliation into standalone and quorum runtime startup; verify imports, updates, removals, missing-store diagnostics, and idempotency through runtime-level integration tests.
- [ ] 8.3 Correct memory deactivation semantics and exclude inactive protocol entries from normal list, query, consolidation, statistics, and retention paths as appropriate.
- [ ] 8.4 Make model overlay application transactional, validate provider availability before activation, and apply selected presets consistently to standalone and quorum runtimes.
- [ ] 8.5 Ensure manifest and protocol value `Debug`, `Display`, and serialization surfaces redact credentials, MCP environment values, and headers.
- [ ] 8.6 Replace helper-only protocol tests with end-to-end tests that construct runtimes and assert durable tasks, scheduler state, startup execution, knowledge retrieval, quorum presets, and secret-safe inspection.
