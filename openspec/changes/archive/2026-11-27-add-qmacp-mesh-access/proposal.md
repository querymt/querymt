# Proposal: Manage mesh access from qmacp

## Why

Mesh access management must be usable and testable without the desktop UI or an attached session. qmacp should expose the v1 mesh access ACP workflow without granting the client any additional authority.

## What Changes

- Add `qmacp mesh` commands to discover nodes, read directional access, set a grant, set or reset provider/model quotas, and wait for a scoped access change.
- Preserve the existing JSON output and exit codes. Require an explicit revision for every write and never retry an ambiguous mutation automatically.

## Non-goals

Server policy or authentication changes, UI work, mesh invite administration, and switching a client's transport security off for administration.

## Capabilities

### New Capabilities

- `qmacp-mesh-access`: a CLI for the ACP v1 mesh access methods from `add-mesh-peer-access`.

## Impact

- `qmacp` (default features): additive CLI subcommands. No session DB migration, provider contract, TOML config, or desktop work. The runtime's separate ACP change is a prerequisite for live server tests.
