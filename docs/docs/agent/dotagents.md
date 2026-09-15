# QueryMT Agent - `.agents` Protocol Support

QueryMT Agent can consume portable `.agents/` Protocol configuration: instructions, prompts, MCP servers, model presets, skills, sub-agents, repeat tasks, and memories. The same files work across tools, so shared configuration does not need to be duplicated into QueryMT-specific TOML.

Protocol support is **opt-in**. When it is disabled, or when no protocol directory exists, QueryMT behaves exactly as before and never creates a `.agents` directory.

## Enabling

### Via TOML

Add a `[dotagents]` section to a single-agent or quorum configuration:

```toml
[dotagents]
enabled = true
# Optional: select a preset from models.json
selected_model_preset = "fast"
# Optional: scope the workspace layer explicitly
# workspace = "/path/to/repo"
# Optional: override layer roots
# global_root = "/home/me/.agents"
# workspace_root = "/path/to/repo/.agents"
global_enabled = true
workspace_enabled = true
# Optional: additional explicitly trusted roots for confined resolution
# trusted_roots = ["/srv/shared-skills"]
# "compatibility" (default) or "strict"
strictness = "compatibility"
# Trust policy for repository-controlled workspace tasks
workspace_task_trust = "prompt"
```

All fields are optional and default to preserving current behavior. `enabled` defaults to `false`; `global_enabled` and `workspace_enabled` default to `true` and only apply once `enabled` is set.

### Programmatically

```rust
use querymt_agent::dotagents::{DotagentsLoadOptions, DotagentsTaskTrustPolicy};

let options = DotagentsLoadOptions::enabled()
    .with_workspace("/path/to/repo")
    .with_global_enabled(false)
    .with_selected_model_preset("fast")
    .with_workspace_task_trust(DotagentsTaskTrustPolicy::Prompt);

let agent = Agent::single()
    .provider("anthropic", "claude-sonnet-4-5-20250929")
    .dotagents_options(options)
    .build()
    .await?;
```

A pre-resolved manifest can be supplied instead, which skips discovery entirely:

```rust
let agent = Agent::single()
    .provider("anthropic", "claude-sonnet-4-5-20250929")
    .dotagents_manifest(manifest)
    .build()
    .await?;
```

### Previewing Without Side Effects

Resolution is side-effect free. `preview_dotagents_manifest` applies the same precedence as `build` but starts no MCP server, persists or schedules no task, imports no memory, and creates no directory:

```rust
let manifest = builder.preview_dotagents_manifest()?.expect("layers resolved");
for (id, server) in &manifest.mcp_servers {
    println!("{id}: {}", server.transport);
}
for diagnostic in &manifest.diagnostics {
    eprintln!("{diagnostic}");
}
```

## Supported Directory Layout

```text
<root>/
  agents.md                  # AGENTS.md-compatible instructions (singleton)
  system-prompt.md           # system prompt (singleton)
  mcp.json                   # MCP servers, keyed by mcpServers entry name
  models.json                # model presets, keyed by preset name
  skills/<id>/skill.md       # or SKILL.md; keyed by id or directory name
  agents/<id>/agent.md       # sub-agent profile; keyed by id or directory name
  agents/<id>/config.json    # adjacent supported settings
  tasks/<id>/task.md         # repeat task; keyed by id or directory name
  memories/<id>.md           # memory; keyed by id or file stem
```

`<root>` is the global layer `~/.agents/` or the workspace layer `<workspace>/.agents/`.

## Precedence

Layers are applied in this order, each overriding the previous:

1. explicit QueryMT configuration or programmatic builder state;
2. global protocol layer (`~/.agents/`);
3. workspace protocol layer (`<workspace>/.agents/`).

Merge rules depend on the document type:

| Content | Rule |
|---------|------|
| `agents.md`, `system-prompt.md` | Singleton: the workspace document replaces the global one |
| `mcp.json`, `models.json` | Top-level keys merge; a workspace key replaces the matching global key |
| `skills/`, `agents/`, `tasks/`, `memories/` | Entries merge by normalized ID; a workspace entry replaces the matching global entry |

Unmatched global content always remains available. Within a single layer, duplicate normalized IDs are an error rather than a filesystem-order-dependent winner.

Protocol files only affect the fields they represent. Everything else in the explicit configuration is preserved.

## Prompts

Protocol prompts are **additive** to explicit system parts, in this order:

1. explicit QueryMT system parts;
2. `system-prompt.md` body;
3. `agents.md` body.

Frontmatter is never sent to the model. The composed prompt is persisted in the session LLM configuration, so model switching and remote forwarding retain it.

## MCP Servers

`mcp.json` entries under `mcpServers` map onto QueryMT's existing MCP client lifecycle:

| Protocol transport | QueryMT transport |
|--------------------|-------------------|
| `stdio` | process-based stdio MCP server |
| `streamable-http` | RMCP streamable HTTP MCP server |

Command arguments, environment values, URLs, and supported headers are preserved. Environment and header values support QueryMT's `${VAR}` interpolation, performed at activation time rather than when displaying the manifest; resolved secret values are redacted in public views and diagnostics.

When `transport` is omitted it is inferred only if unambiguous:

- `command` without `url` implies `stdio`;
- `url` without `command` implies `streamable-http`;
- both or neither is an error requiring an explicit transport.

WebSocket and unknown transports are reported as diagnostics and are **not started**.

## Model Presets

`models.json` presets provide `provider`, `model`, optional credential, and provider parameters. Selecting a preset through `selected_model_preset` overlays only the LLM fields it represents, so unrelated explicit settings survive.

Presets do not install providers and do not replace profile provider locks. An unknown preset, an unavailable provider, or a missing model identifier produces an actionable diagnostic and leaves the explicit base configuration in effect.

## Skills

Skill discovery continues to cover configured sources, `.skills`, `.claude/skills`, `.qmt/skills`, and `.agents/skills`. Inside `.agents/skills`, both protocol `skill.md` and existing `SKILL.md` are accepted; protocol `id`, `name`, `description`, and `enabled` metadata are honored, and the directory name is the ID when `id` is absent. Disabled skills are not exposed to the skill tool.

## Sub-Agents

Protocol `agents/<id>/agent.md` profiles with role `delegation-target` and a supported internal connection become delegation targets:

- a **standalone** agent with delegation enabled gains registry targets;
- a **quorum** profile with delegation enabled gains additional delegates.

The configured planner and configured delegates are never replaced.

Protocol loading **never enables delegation**. When delegation is disabled, protocol targets remain inspectable in the manifest but are not registered.

### Collision Rules

Protocol profile IDs merge across layers first (workspace wins). The merged protocol targets are then combined with explicit QueryMT targets, where **explicit configuration wins**:

1. global/workspace protocol precedence is applied;
2. explicit standalone registry targets and quorum delegates take precedence over matching protocol IDs;
3. the colliding protocol target is skipped and a diagnostic naming both sources is emitted.

`stdio`/executable and unknown connection types are never launched: they retain their metadata and emit an unsupported-connection diagnostic.

## Repeat Tasks and Trust

`.agents/tasks/<id>/task.md` entries describe durable schedules with `kind: task`, a stable ID, prompt body, `intervalMinutes`, `enabled`, `runOnStartup`, and optional `profileId`.

**Parsing and preview never persist, schedule, or execute anything.** Reconciling a trusted, enabled task creates one recurring task and one interval schedule bound to the designated automation session and profile. Re-running reconciliation against unchanged files is a no-op: no duplicates are created across restarts.

### Workspace Task Trust

Tasks in a workspace `.agents/tasks/` directory are treated as **untrusted repository content**, because a cloned repository could otherwise silently schedule tool-capable prompts or run them at startup. Before persistence or execution, the host receives an approval request disclosing:

- the canonical workspace;
- task identity (ID and name);
- the schedule (`intervalMinutes`);
- startup behavior (`runOnStartup`);
- the target profile;
- a bounded summary of the prompt that will run;
- the source path.

Approval is bound to the canonical workspace, the normalized task ID, and a fingerprint over all execution-relevant fields.

| Policy | Behavior |
|--------|----------|
| `prompt` (default) | Request approval; remain pending when no approval mechanism exists |
| `deny` | Never activate workspace protocol tasks |
| `allow` (unsafe) | Activate without prompting and emit a prominent diagnostic |

Changing any execution-relevant field invalidates the previous approval: the protocol-owned schedule is paused until the new fingerprint is approved. Revoking trust pauses protocol-owned schedules without touching user-created ones.

Global tasks may use a separately configured policy, because `~/.agents` is user-controlled.

## Memories

`.agents/memories/*.md` documents import idempotently into the configured knowledge store. The body (or explicit `content` metadata) becomes the entry text, the title or a deterministic fallback becomes the summary, `tags` become topics, and the protocol importance value is normalized onto the knowledge score range.

If no knowledge store is configured, memories remain inspectable in the manifest and a non-fatal diagnostic reports that import was skipped. Other protocol features — prompts, skills, sub-agents — are unaffected.

## Security

- **Path confinement**: references resolving outside their owning layer root are rejected before any file is read or process is started. `..` traversal, symlink escapes, and non-regular files are refused.
- **No implicit creation**: workspace discovery only happens when `<workspace>/.agents` already exists.
- **Secrets**: `${VAR}` references are interpolated at activation; diagnostics, debug output, and inspectable manifests redact resolved secret values while retaining enough context to fix errors. Secret values are never used as entry IDs or persisted as reconciliation keys.
- **No executable sub-agents**: `stdio`/executable connection types never launch a process.
- **Task trust**: workspace tasks require explicit, fingerprint-bound approval by default.

## Strictness

| Mode | Behavior |
|------|----------|
| `compatibility` (default) | Activate valid entries, return structured warnings, and fail only when the selected root configuration cannot be applied safely |
| `strict` | Fail resolution when any error diagnostic is present |

Invalid entries are isolated in both modes: a malformed file never hides valid siblings.

## Protocol-to-QueryMT Mapping

| Protocol | QueryMT |
|----------|---------|
| `agents.md` | additional system-prompt part, after explicit parts |
| `system-prompt.md` | additional system-prompt part, before `agents.md` |
| `mcp.json` `stdio` | process-based MCP server |
| `mcp.json` `streamable-http` | RMCP streamable HTTP MCP server |
| `models.json` preset | LLM overlay (provider, model, credential, parameters) |
| `skills/<id>/skill.md` | skill tool source under `.agents/skills` |
| `agents/<id>/agent.md` | delegation registry target |
| `tasks/<id>/task.md` | durable recurring task + interval schedule |
| `memories/<id>.md` | knowledge store entry with a stable protocol source key |

## Dependencies

Protocol support adds **no new dependencies**. It reuses QueryMT's existing JSON/serde, frontmatter, RMCP, skill, profile, delegation, scheduler, and knowledge components.

Two similarly named crates were evaluated as potential helpers and deliberately **excluded**, because neither implements the protocol targeted here:

| Crate | Evaluation | Decision |
|-------|------------|----------|
| `dotagents` (0.1.x) | A binary-only `.dotagents/` configuration deployment and Handlebars templating tool. No library target; different directory (`.dotagents/`), different schema, and a write-oriented workflow. | Not added |
| `agent-runbooks` (0.1.x) | Exposes only initialization of bundled files under `.agent/runbooks/`. Does not parse or model `.agents` Protocol instructions, MCP, models, skills, agents, tasks, or memories. | Not added |

Invoking the `dotagents` executable or copying its internal modules was rejected because it would introduce a distinct configuration standard, write side effects, CLI coupling, and a large dependency surface without reusable protocol semantics. Adopting `agent-runbooks` was rejected because runbooks are not part of the current `.agents` Protocol and the crate provides no relevant parser.

Neither crate is listed in any `Cargo.toml` or present in `Cargo.lock`. The protocol boundary is kept sufficiently isolated that a future authoritative protocol library could replace parsing without changing the runtime adapters.

## Related

- [Configuration](configuration.md) — TOML configuration reference
- [Delegation](delegation.md) — delegation targets and registries
- [Profiles](profiles.md) — profile catalog and session bindings
- [Support Matrix](dotagents_support_matrix.md) — supported vs. unsupported protocol surface
