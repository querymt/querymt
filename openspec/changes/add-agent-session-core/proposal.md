# Proposal

## Why

Every QueryMT client turns the same agent output (the `querymt/sessionLoadSnapshot.v1` load snapshot plus ACP `session/update` notifications) into a conversation view, and each one does it separately: querymt-desktop in about 3,000 lines of TypeScript (`session-updates.ts`, `session-snapshot.ts`, `session-conversation.ts`), and querymt-mobile in its own `sessionUpdateReducer.ts`. The desktop version copies the whole session on most updates and rebuilds every turn on each change, so it slows down as sessions get longer. It also drifts from the agent's wire types, because it re-declares them by hand. One pure, deterministic Rust reducer that is built from the agent's own types and runs natively and as WebAssembly fixes both problems at the source.

The wire types the reducer needs (`AgentEvent`, `AgentEventKind`, the load-snapshot payload and their closure) currently live in `querymt-agent-core`, `querymt-agent-services` and `querymt-agent-runtime`. These crates pull in tokio, async-trait, rusqlite contracts, the ACP runtime and more, so a browser build cannot depend on them. Those types move into a light crate that both the agent and the reducer depend on, so drift becomes a compile error instead of a runtime bug.

## What Changes

- **ACP stays the protocol.** `agent-session-core` is a client-side consumer of ACP. It adds and changes no wire message, request or capability, and agents and clients keep talking ACP exactly as today.
  - Live `session/update` notifications are decoded with the official ACP schema types. That includes context compaction (`compaction_update`, `compaction_summary_chunk`, which a client opts into through the standard `session.compaction` capability, `CompactionCapabilities`) and plan operations (`plan_update`, `plan_removed`).
  - The only QueryMT extension the reducer reads is the existing `_meta["querymt/sessionLoadSnapshot.v1"]` on the `session/load` response, an ACP-sanctioned `_meta` extension. Without it, the reducer builds the view from the standard replayed `session/update` notifications.
  - The reducer's input list (`snapshot_loaded`, `acp_notification`, `prompt_optimistic`, …) is an in-process API between a client's transport and the reducer, not a wire protocol.
- **New package `querymt-agent-protocol-types`** (`crates/agent-protocol-types`). It holds the plain wire types clients consume:
  - agent events and their leaf types,
  - the load-snapshot payload and its parts (audit view, stream cursor, accounting snapshot, history window info, summary, truncation entries, delegation updates, user prompt records, runtime status),
  - session input-state notifications.
  
  It is serde and typeshare only, has no async runtime, and builds for `wasm32-unknown-unknown`.
- **New package `querymt-agent-session-core`** (`crates/agent-session-core`). It is a pure reducer: session events in, a session view (header plus ordered conversation turns) and a change report out. It covers:
  - snapshot hydration, live ACP updates, older-history pages, optimistic prompts, runtime and input-state observations, undo stacks, delegation updates and config options,
  - deterministic output with no clock or randomness,
  - tolerant decoding,
  - a bounded debug event log (newest 1,000 entries).
- **New package `querymt-agent-session-core-wasm`** (`crates/agent-session-core-wasm`, `publish = false`). These are wasm-bindgen bindings that keep the state inside the module and exchange JSON batches and change reports.
- **Release artifacts.** Nightly and stable releases publish `agent-session-core-wasm-<version>.tar.gz` plus its `.sha256`, listed in `release.json`.
- **Toolchain.** `rust-toolchain.toml` gains the `wasm32-unknown-unknown` target. The devShell gains `wasm-bindgen-cli` (matching the locked `wasm-bindgen` 0.2.129) and `binaryen`.
- **`querymt` features.** The `which` dependency becomes optional and is enabled by the `mcp` feature, which is its only user. As a result, `querymt` with `default-features = false` builds for `wasm32-unknown-unknown`.
- **Typeshare.** `scripts/generate-types.sh` reads the two new crates. Declarations of moved types are unchanged. The new session event and view types are added.
- **BREAKING (Rust API):**
  - `impl From<McpServerConfig> for McpServerInfo` is replaced by `McpServerConfig::server_info()` (orphan rule after the move).
  - Inherent constructors on moved types that need agent logic become functions in the owning crate. For example, `AccountingSnapshot::from_reducer` / `from_events` become `querymt_agent_services::accounting::snapshot_from_reducer` / `snapshot_from_events`.
  - Module paths under `querymt_agent_core` keep resolving through re-exports, so other paths do not change.
- No change to ACP or provider wire shapes, the session DB schema, config, hooks, middleware, the Extism contract, Python bindings or the log targets of the agent crates.

## Non-goals

- Replacing ACP, defining new ACP messages, or adding QueryMT-specific session update kinds. Compaction and plans stay on the standard ACP updates.
- Switching the agent runtime, querymt-mobile-ffi, querymt-mobile or querymt-ios to the new reducer. They can adopt it later.
- Markdown or HTML rendering, diff building, or tool presentation. These stay in clients.
- Changing what the agent emits. The reducer consumes today's snapshot and notifications.
- Publishing the crates to crates.io or an npm package.
- Splitting `querymt-agent-core` further than the wire-type extraction.

## Capabilities

### New Capabilities
- `session-view-state`: deterministic reduction of agent session output (load snapshot, ACP session updates, history pages and client observations) into a client conversation view and change reports, available natively and as a published WebAssembly module.

### Modified Capabilities

None.

## Impact

**Packages and features:**

| Package | Status | Features whose code changes |
|---|---|---|
| `querymt-agent-protocol-types` | New | none |
| `querymt-agent-session-core` | New | none |
| `querymt-agent-session-core-wasm` | New, unpublished | none |
| `querymt` | `which` becomes optional | `mcp` gains `dep:which` |
| `querymt-agent-core` | Moved modules become re-exports; `McpServerConfig::server_info()` | none |
| `querymt-agent-services` | Accounting snapshot and history-window payload types move out; constructors become functions | none |
| `querymt-agent-runtime` | `SessionLoadSnapshot`, `UserPromptRecord` and input-state notification types move out | none |
| `querymt-agent`, `querymt-mobile-ffi`, `querymt-agent-py` | Downstream; no source change expected | none |

**Typeshare:** the set of declarations for existing types is unchanged. New declarations cover the session event, view and change-report types.

**Coordination:** this builds on `split-agent-crate` (groups 1–7 are landed; it must rebase or run after group 8 to avoid churn on the facade). The layering-check mapping (`scripts/check-agent-layering.py`, `agent-layering-allowance.json`) gets the new crate if the check still exists.

**querymt-desktop follow-up (UI, not planned here):** the desktop change `adopt-agent-session-core` vendors the WASM artifact, regenerates `src/lib/querymt/generated/types.ts`, and replaces its TypeScript reducer and conversation builder with this module.
