# Design

## Context

pi-lens has a Pi-native JavaScript adapter, a persistent MCP server, and `pi-lens-analyze --hook`. QueryMT already supports MCP servers and command hooks, but its hook output schema is snake_case and automatic analysis currently lacks authoritative changed paths. This change follows `add-post-tool-file-effects`; dynamic activation uses `add-dynamic-tools-for-querymt` when available.

## Goals / Non-Goals

**Goals:**
- Reuse pi-lens's shipped non-Pi adapters with minimal compatibility code.
- Provide push diagnostics backed by warm pi-lens state and expose supported MCP tools.
- Keep integration failures advisory.

**Non-Goals:**
- Load `dist/index.js` or emulate Pi's complete `ExtensionAPI`.
- Reimplement pi-lens analysis in Rust.
- Support Pi widgets, renderers, notifications, or slash commands.
- Vendor or automatically trust Node packages.

## Decisions

### Compose MCP and command hooks

Configure `pi-lens-mcp` as the long-lived service and `pi-lens-analyze --hook` as the automatic PostToolUse adapter. The command can use pi-lens's workspace IPC path to reach warm state and retains pi-lens's documented cold fallback.

Using only MCP was rejected because it is pull-oriented and does not automatically analyze changes. Loading the Pi-native extension was rejected because it requires a broad JavaScript host compatibility layer.

### Drive analysis from QueryMT file effects

Add a small QueryMT-side hook adapter that expands the normalized changed-file set into deterministic per-file pi-lens hook invocations. It filters removed and unsupported paths, maps QueryMT's `filePath` and arbitrary tool changes to the path fields pi-lens accepts, deduplicates paths, and merges bounded findings.

Prefer adding upstream pi-lens support for QueryMT's `file_changes` array when feasible; retain the adapter so QueryMT does not depend on a particular pi-lens release adding that field.

### Accept both hook naming conventions

Add Serde aliases for `hookSpecificOutput`, `hookEventName`, `additionalContext`, and relevant nested output fields while keeping existing snake_case serialization canonical. Event-name validation accepts the documented semantic equivalents for PostToolUse. Compatibility is scoped to parsed output; QueryMT's input format remains versioned and documented.

### Use fail-open observational handlers

Add a handler error policy with `continue` for diagnostics. Spawn failures, non-zero exits, timeout, and parse errors become `HookNotice`s and preserve the original result. Existing blocking/security hooks retain fail-closed behavior unless explicitly configured otherwise.

### Map only supported lifecycle operations

Session startup initializes the MCP service and, where available, calls pi-lens session-start. QueryMT Stop invokes bounded turn-end processing; SessionEnd invokes shutdown. Lifecycle tools that are not exposed by a pi-lens version are treated as optional capabilities discovered from its MCP tool list.

### Package integration as configuration, not a Rust dependency

Provide a documented preset/example with executable paths, timeouts, context limits, matcher, and tool policy. Installation remains explicit through npm or another user-managed mechanism. QueryMT validates executable availability but does not run package-manager installation scripts.

## Risks / Trade-offs

- [pi-lens protocol changes] -> Pin/test a documented compatibility range and feature-detect MCP tools.
- [One process per changed file adds latency] -> Use the warm IPC fast path, deterministic concurrency limits, and encourage upstream batch input support.
- [Cold fallback omits LSP findings] -> Preserve pi-lens's explicit skipped/deferred signal rather than reporting a false clean result.
- [Large diagnostics consume context] -> Enforce per-handler and aggregate byte/item caps with truncation notices.
- [Stop processing delays completion] -> Use a separate bounded timeout and remain fail-open.

## Migration Plan

1. Land `add-post-tool-file-effects` and verify changed paths in PostToolUse.
2. Add hook output aliases and observational error policy behind configuration.
3. Add pi-lens example configuration and executable capability checks.
4. Add integration tests using a protocol fixture, then opt-in documentation for the real npm package.
5. Enable dynamic activation integration after `add-dynamic-tools-for-querymt`; static MCP exposure remains the fallback.
6. Roll back by disabling/removing the pi-lens profile; no session database migration is required.
