# Design

## Context

Built-in tools are stored in an immutable configuration registry. MCP tools already use an `ArcSwap` snapshot and refresh after `tools/list_changed`, but QueryMT has no source-neutral session API that separates registration from model visibility. See `proposal.md` and `specs/dynamic-tool-availability/spec.md`.

## Goals / Non-Goals

**Goals:**
- Provide one consistent per-session effective tool snapshot across sources.
- Support activation/deactivation without invalidating in-flight calls.
- Reuse existing policy and MCP behavior.

**Non-Goals:**
- Load JavaScript extensions or arbitrary dynamic libraries.
- Allow tools to bypass session policy.
- Change the MCP protocol or require all tools to be dynamic.

## Decisions

### Maintain a session-scoped catalog and immutable generations

Add a session tool catalog containing source-qualified registrations, activation state, and a monotonically increasing generation. Every mutation builds and atomically publishes an immutable effective snapshot containing definitions and executable adapters. Model requests capture one snapshot; selected calls retain an `Arc` to their adapter.

This generalizes the consistency property of `McpToolState`. Mutating the configuration-level `ToolRegistry` was rejected because it is shared and cannot safely represent per-session state.

### Separate registered, requested-active, and effective-visible state

A tool may be registered and requested active yet filtered by policy. Effective visibility is derived from registration, activation, and current policy. This avoids treating deactivation as deletion and makes policy changes recomputable.

Built-in and provider tools begin registered and active to preserve behavior. MCP tools begin active unless configuration says otherwise. Runtime registrations choose their initial state explicitly.

### Use source-qualified identity with globally unique model names

Internally identify tools by source and local name. Model-visible names remain globally unique. Registration that collides with another source fails unless an explicit host-controlled replacement operation is used. No last-writer-wins behavior is allowed.

### Apply mutations at model-request boundaries

Mutation requests update the pending catalog immediately, but the execution loop publishes the resulting snapshot only at a safe boundary before a model request. A batch API groups related mutations into one generation. This mirrors Pi's expectation that `setActiveTools` changes next-turn visibility rather than changing the current prompt.

### Fold MCP refresh into the catalog

`tools/list_changed` replaces the registrations owned by that MCP server in one transaction. Existing MCP adapters and notification handling remain; only publication moves through the common catalog. This prevents parallel snapshots from diverging.

### Expose narrow runtime operations

Provide session operations to list registrations, register/unregister authorized runtime tools, and set active names. The API reports collisions, unknown names, and policy-filtered names. Tool-set events include generation and effective definitions/hash, not executable objects.

## Risks / Trade-offs

- [More state than the current registry] -> Centralize derivation in one catalog and test generation transitions exhaustively.
- [Stale calls after removal] -> Keep adapter generations alive with `Arc`; removal affects discovery only.
- [Tool schema changes increase prompt churn] -> Hash effective definitions and publish only real changes.
- [Provider tools may not have stable adapters] -> Capture provider execution routing in the generation rather than resolving by mutable name later.
- [Collision behavior breaks implicit overrides] -> Reject collisions with actionable diagnostics and require explicit host authority for replacement.

## Migration Plan

1. Introduce the catalog initialized from existing built-in, provider, and MCP sources while preserving the current visible set.
2. Route tool collection and execution lookup through captured catalog snapshots.
3. Move MCP refresh into source-scoped catalog transactions.
4. Add runtime mutation APIs and effective-set events.
5. Remove redundant MCP-only publication state after parity tests pass.
6. Roll back by disabling runtime mutations and initializing one static generation; no persisted-data migration is required.
