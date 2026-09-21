# Tasks

## 1. Session Tool Catalog

- [ ] 1.1 Define source-qualified tool identities, registration records, activation state, generation metadata, and immutable effective snapshots, and verify catalog unit tests cover registered/inactive/effective states
- [ ] 1.2 Initialize each session catalog from built-in, provider, and MCP tools while preserving the current default visible set, and verify parity tests compare old and new collected definitions
- [ ] 1.3 Implement deterministic collision detection and explicit replacement authority, and verify duplicate names never silently replace another source

## 2. Atomic Visibility and Execution

- [ ] 2.1 Add batched register, unregister, activate, and deactivate operations with pending state and safe-boundary publication, and verify model requests see only complete generations
- [ ] 2.2 Capture the effective generation for each model request and retain executable adapters for selected calls, and verify a call completes after concurrent deactivation or removal
- [ ] 2.3 Route model tool-definition collection and execution lookup through the captured session snapshot, and verify concurrent request/mutation tests do not mix definitions and adapters

## 3. Policy and MCP Integration

- [ ] 3.1 Apply allow/deny, capability, permission, and argument-validation policy while deriving effective tools, and verify denied dynamic tools remain hidden and non-invocable
- [ ] 3.2 Convert each MCP `tools/list_changed` refresh into one source-scoped catalog transaction, and verify additions, removals, and schema changes appear atomically
- [ ] 3.3 Preserve requested activation across compatible MCP refreshes and remove registrations absent from the server, and verify refresh tests cover active and inactive tools

## 4. Runtime API and Events

- [ ] 4.1 Add session operations to list registrations and perform authorized batched mutations, and verify unknown names, collisions, and policy-filtered outcomes return actionable responses
- [ ] 4.2 Emit generation-aware tool-availability events only when the effective hash changes, and verify idempotent activation produces no event
- [ ] 4.3 Document next-request visibility and source/collision semantics, and verify public examples demonstrate register-inactive then activate

## 5. Verification

- [ ] 5.1 Add integration tests spanning built-in, provider, MCP, and runtime tools, including mutations during an in-flight request and call
- [ ] 5.2 Run agent protocol, MCP refresh, tool policy, execution, formatting, and lint test suites, and record any unrelated failures
