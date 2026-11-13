# Tasks

## 1. Prerequisites and Compatibility Contract

- [ ] 1.1 Confirm `add-post-tool-file-effects` is implemented and verify a PostToolUse fixture receives normalized changes from a shell `sed` invocation
- [ ] 1.2 Define and document the supported pi-lens version range, executable capabilities, lifecycle tools, and unsupported Pi-native surfaces, and verify the compatibility matrix names MCP, hooks, UI, commands, flags, and event-bus behavior
- [ ] 1.3 Add camelCase aliases for the documented pi-lens/Claude PostToolUse output envelope while retaining canonical snake_case support, and verify parser tests accept both forms and reject unrelated fields

## 2. Observational Hook Behavior

- [ ] 2.1 Add a configurable fail-open handler error policy that converts spawn, exit, timeout, and parse failures into notices, and verify a successful tool result stays successful for each failure mode
- [ ] 2.2 Add a pi-lens changed-file adapter that filters unsupported/removed paths, deduplicates and orders files, invokes `pi-lens-analyze --hook`, and merges bounded non-empty findings, and verify multi-file and clean-result tests
- [ ] 2.3 Enforce per-file concurrency, aggregate deadline, item, and byte limits, and verify oversized or slow diagnostics produce deterministic truncation/timeout notices without blocking the agent

## 3. Persistent MCP and Lifecycle

- [ ] 3.1 Add an opt-in pi-lens MCP configuration profile that starts `pi-lens-mcp` for the workspace, and verify its advertised tools are callable through QueryMT's existing MCP client
- [ ] 3.2 Feature-detect and invoke available pi-lens session-start, turn-end, and session-end operations at QueryMT lifecycle boundaries with bounded fail-open behavior, and verify absent optional tools do not fail startup
- [ ] 3.3 Verify the command hook reaches pi-lens's warm workspace IPC path when the MCP service is running and preserves the explicit cold/no-LSP fallback signal when it is not
- [ ] 3.4 After `add-dynamic-tools-for-querymt` is available, map pi-lens tool activation changes to next-request visibility and verify static MCP exposure remains a supported fallback

## 4. Configuration and Documentation

- [ ] 4.1 Add executable availability validation and actionable notices without invoking a package manager, and verify a missing Node or pi-lens installation leaves the session usable
- [ ] 4.2 Add a complete opt-in configuration example for MCP, PostToolUse, Stop, timeouts, context limits, and tool policy, and verify it parses with QueryMT's configuration loader
- [ ] 4.3 Document installation, trust implications, warm versus cold diagnostics, changed-file behavior for arbitrary tools, supported tools, and removal/rollback steps

## 5. End-to-End Verification

- [ ] 5.1 Add a deterministic fake pi-lens MCP/hook fixture and verify automatic diagnostics, clean edits, `sed` changes, process crashes, malformed output, timeouts, and lifecycle shutdown
- [ ] 5.2 Run an opt-in smoke test against the supported real pi-lens package version and verify MCP tool discovery plus warm post-edit diagnostics without making network-dependent tests part of the default suite
- [ ] 5.3 Run hook, MCP, lifecycle, tool-availability, configuration, formatting, and lint test suites, and record any unrelated failures
