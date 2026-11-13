# Proposal

## Why

pi-lens already ships host-neutral MCP and command-hook adapters, but QueryMT cannot yet provide reliable changed-file input to its automatic analysis hook or consume its Claude-compatible hook output unchanged. Supporting those existing adapters gives QueryMT warm LSP diagnostics and pi-lens tools without embedding pi-lens or implementing Pi's JavaScript extension API.

## What Changes

- Add a documented QueryMT integration profile for the separately installed `pi-lens` package.
- Start `pi-lens-mcp` as a persistent session MCP server so its model-facing tools and warm LSP state are available.
- Invoke `pi-lens-analyze --hook` from `PostToolUse` for internally detected changed files rather than relying on edit-tool names or argument shapes.
- Accept the camelCase Claude hook output fields emitted by pi-lens while retaining QueryMT's existing snake_case hook format.
- Treat automatic pi-lens diagnostics as observational: timeout, absence, or malformed output produces a notice and does not fail a successful tool call.
- Map QueryMT session and turn lifecycle to the supported pi-lens MCP/hook lifecycle where required, with bounded execution and clean shutdown.
- Document unsupported native-Pi surfaces such as widgets and Pi slash commands rather than emulating the full Pi `ExtensionAPI`.

This change depends on `add-post-tool-file-effects` for source-agnostic changed-file delivery. It also uses `add-dynamic-tools-for-querymt` for runtime activation semantics where pi-lens changes the model-visible tool set; static MCP tool exposure remains usable without activation.

## Capabilities

### New Capabilities

- `pi-lens-integration`: Defines installation, configuration, lifecycle mapping, hook compatibility, changed-file analysis, MCP tool exposure, and graceful degradation for pi-lens.

### Modified Capabilities

None.

## Impact

- Affects hook wire compatibility and error policy under `crates/agent/src/hooks/`.
- Adds configuration/examples and integration tests for the external `pi-lens` npm package.
- Exercises QueryMT's existing MCP client and the capabilities introduced by the two prerequisite changes.
- Adds no pi-lens, Node, LSP, or language-server library dependency to the QueryMT Rust crates; users install pi-lens separately.
- Does not load `pi-lens/dist/index.js` or implement Pi's in-process JavaScript extension API.
