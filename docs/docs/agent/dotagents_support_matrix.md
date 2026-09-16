# `.agents` Protocol Support Matrix

This page states exactly which parts of the `.agents` Protocol QueryMT Agent supports. Anything listed as unsupported is **detected and diagnosed** rather than silently ignored, so the extent of support is always inspectable.

## Top-Level Protocol Entries

| Entry | Status | Notes |
|-------|--------|-------|
| `agents.md` | Supported | Instructions appended after explicit system parts; frontmatter excluded |
| `system-prompt.md` | Supported | Appended after explicit parts and before `agents.md` |
| `mcp.json` | Supported | `stdio` and `streamable-http` transports only |
| `models.json` | Supported | Model presets as LLM overlays; does not install providers |
| `skills/` | Supported | `skill.md` and `SKILL.md`; existing sources preserved |
| `agents/` | Supported | Internal `delegation-target` profiles; extends delegation registries |
| `tasks/` | Supported | Reconciles to durable recurring tasks and interval schedules, subject to trust |
| `memories/` | Supported | Idempotent import into the configured knowledge store |
| `speakmcp-settings.json` | Not supported | Detected and reported; never interpreted |
| `layouts/` | Not supported | Layout preferences are out of scope; detected and reported |
| `.backups/` | Not supported | Backup rotation is out of scope; detected and reported |
| any other top-level entry | Not supported | Reported as an unknown protocol artifact |

## MCP Transports

| Transport | Status | Behavior |
|-----------|--------|----------|
| `stdio` | **Supported** | Started through QueryMT's existing process-based MCP transport |
| `streamable-http` | **Supported** | Connected through QueryMT's existing RMCP streamable HTTP transport |
| `websocket` | Not supported | Never started; a diagnostic identifies WebSocket as unrepresentable |
| unknown/future transport | Not supported | Never started; a diagnostic names the server and transport |

Transport spellings are matched case-insensitively, and `_`/`-` variations are accepted:

| Accepted spelling | Resolves to |
|-------------------|-------------|
| `stdio` | stdio |
| `streamable-http`, `streamablehttp`, `http` | streamable-http |
| `websocket`, `ws`, `wss` | websocket (unsupported) |
| anything else | unknown (unsupported), diagnosed with the raw spelling |

Transport inference applies only when `transport` is omitted **and** the structural fields are unambiguous:

| Fields present | Inferred |
|----------------|----------|
| `command` only | `stdio` |
| `url` only | `streamable-http` |
| both `command` and `url` | Error: requires an unambiguous supported transport |
| neither | Error: no supported transport can be determined |

Note that querymt's TOML configuration spells the HTTP transport `http` (its `[[mcp]]` discriminator), while the protocol document accepts `streamable-http`; the adapter translates between them.

## Sub-Agent Connection Types

| Connection | Status | Behavior |
|------------|--------|----------|
| `internal` | Supported | Materialized lazily as a local delegation target |
| `stdio` / executable | Not supported | Never launched; retained with an unsupported-connection diagnostic |
| unknown connection type | Not supported | Never launched; diagnosed with the profile and connection type |

MCP transport support for a sub-agent is a **separate** concern from the sub-agent's own ACP or executable connection type.

## Out of Scope

The following are intentionally not implemented:

- **Hub operations** — publishing and installing `.agents` bundles.
- **Write-back** — QueryMT never rewrites protocol files.
- **Bundle creation** — no `.agents` bundle assembly.
- **Backup rotation** — `.backups/` is not managed.
- **`speakmcp-settings.json`** — not interpreted.
- **`layouts/`** — layout preferences are not interpreted.
- **External sub-agent execution** — arbitrary executables are not launched, because the crate has no sandboxed client-side ACP process lifecycle.

## Preserved Behavior

Enabling protocol support does not change existing behavior:

- current TOML loaders and programmatic builders work unchanged;
- existing skill sources (`.skills`, `.claude/skills`, `.qmt/skills`, configured paths) keep working;
- profile catalogs and session bindings are unaffected;
- session model switching and remote system prompts retain the composed prompt.

When protocol loading is disabled, or no protocol directory exists, no `.agents` directory is ever created.

## Related

- [`.agents` Protocol](dotagents.md) — full support guide
- [Delegation](delegation.md) — delegation targets and registries
