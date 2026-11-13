# Design

## Context

See proposal.md for motivation. Current state that shapes the approach:

- **Skill tool is agent-level.** `build_skill_tool` (`skills/mod.rs`) adds one `SkillTool` to the agent's `ToolRegistry`. Callers are `api/agent.rs` (standalone), `api/quorum.rs` (planner and configured delegates). To list project skills for the session's workspace, the tool overrides `Tool::definition_for_cwd`, and `ToolRegistry::definitions_for_cwd` threads the cwd through. This is the only implementor.
- **MCP is per session.**
  - `build_mcp_state` / `merge_connected_mcp_peers` (`agent/protocol.rs`) connect servers for each session. Sources are agent config, `.agents/mcp.json` and ACP client servers.
  - They publish an `McpToolSnapshot` into the session's `Arc<McpToolState>` (`ArcSwap`).
  - `McpClientHandler::on_tool_list_changed` (`elicitation.rs`) rebuilds one server's slice of that snapshot with a `load()` followed by a `store()`. Two concurrent refreshes can therefore lose an update.
- **Tool use is filtered per turn.**
  - Each turn's `ExecutionContext` carries the session's `ToolConfig` snapshot (policy, allowlist, denylist).
  - `collect_tools` builds the model-facing list from the builtin registry plus the MCP snapshot.
  - `is_mcp_tool_allowed_with` (`agent/tools.rs`) gates MCP tools by `server.tool` / `server.*` allowlist selectors. Its denylist check only matches the bare tool name, so a `server.*` denylist entry does not hide anything today.
  - The tools hash compared in `transitions.rs` covers every collected definition.
- **MCP server names are not validated.** A name may contain `.`, which makes `server.name` selectors ambiguous.
- **Skill permissions.** `SkillPermissions::check` tries the exact name, then iterates a `HashMap` for prefix wildcards (first match wins, so overlapping wildcards resolve non-deterministically), then `"*"`, and defaults to `Allow`.
- **Approval prompts.** `ToolContext::ask_question` sends an elicitation that the turn loop registers in `AgentConfig::pending_elicitations` and announces on that config's event sink. The elicitation ID is `skill-permission-<name>`; this change appends a UUID (see §7).
- **Sub-agents.**
  - `.agents` delegation targets are built by `DotagentsTargetFactory::create` (`api/dotagents_targets.rs`). They get no skill tool. Their `tools` restriction forces `ToolPolicy::BuiltInOnly`, even when the plan attaches MCP servers. They get their own `AgentConfig` (own pending-elicitation map and event sink) and no event fanout, so a prompt raised in a target session is not routed to the user.
  - Configured quorum delegates infer `BuiltInAndProvider` when they have MCP selectors or servers, and share the quorum's event fanout.
  - Policy inference exists in three slightly different copies: `api/agent.rs` (dot or bare server name, or attached servers), and two in `api/quorum.rs` (dot, or attached servers).
- **rmcp 3.9 client API.**
  - Discovery: `peer_info().capabilities.supports_skills()`, `Peer::list_all_skills`, `Skill::validate`.
  - Loading: `Peer::get_skill`, `Peer::read_resource`, `SkillResources::{Files, Dynamic}`, `SkillResource::verify`.
  - Clients declare nothing to use the extension. `list_skills`/`get_skill` fail locally with `-32601` when the server did not declare it.
  - Under protocol 2026-07-28, `resources/subscribe` is legacy and `Peer::listen(SubscriptionFilter)` opens notification subscriptions. Notifications routed to a `Subscription` are not delivered to `ClientHandler` callbacks. The existing harness test (`mcp_harness.rs`, `list_changed_bypasses_the_cache_and_updates_tool_state`) shows that an unsolicited `tools/list_changed` from a 2026-07-28 server still reaches `on_tool_list_changed`.

## Goals / Non-Goals

**Goals:**
- One `skill` tool whose content is derived from the session: its workspace, its MCP connections and its turn tool configuration.
- No cwd/session special cases on the generic `Tool` trait.
- MCP skill state lives next to MCP tool state, so all MCP refresh paths share one snapshot, updated atomically.
- Delegated sub-agents get the same skill behaviour through the same mechanism, not a separate path, and never hang on an approval nobody can see.
- Server-supplied text is treated as untrusted: bounded, verified where possible, and unable to break out of its wrapper.

**Non-Goals:**
- `resources/directory/read`.
- Serving skills as an MCP server.
- Resource subscriptions (`resources/updated`) for individual skill files.
- Caching `skills/get` results across loads.
- Applying `allowed-tools`.
- Showing MCP skills in the `.agents` manifest preview.
- Fixing the pre-existing gap where a `server.*` denylist entry does not hide MCP *tools*. The new server-visibility helper makes the fix a one-line follow-up, but changing MCP tool filtering belongs to the `mcp-tool-invocation` capability and a separate change.
- Forwarding approval prompts from `.agents` delegation targets to the parent's user.

## Decisions

### 1. Agent-level `SkillCatalog`, session-bound `SkillTool`

`SkillCatalog` (in `skills/`) holds:
- the resolved local sources and `include_external`;
- the permissions;
- the `mcp` switch;
- the approval mode (`Interactive` by default, `NonInteractive` for `.agents` targets, see §7);
- the existing per-workspace `SkillRegistry` cache;
- the set of collision pairs already warned about (§4);
- the injectable discovery function used by tests.

All of these move out of today's `SkillTool`. `AgentConfig` gains `skill_catalog: Option<Arc<SkillCatalog>>`, set through `AgentConfigBuilder::with_skill_catalog`. `build_skill_tool` becomes `SkillCatalog::from_config(&SkillsConfig, project_root)`.

`SkillTool` becomes a cheap value bound to one session and one turn:
- `Arc<SkillCatalog>`;
- the session workspace at turn start (runtime cwd, falling back to the catalog's project root), used for the schema;
- the session's `Arc<McpToolState>`;
- the turn's `ToolConfig`.

It implements the plain `Tool::definition()` and `call()`. `call()` resolves local skills against `ctx.cwd()` (falling back to the bound workspace), as today, so a mid-turn worktree or checkout switch loads from the current workspace. The schema stays the turn-start snapshot; a skill that exists only in the new workspace still loads through the existing refresh-on-miss path.

Alternatives considered:
- **Pass session context into `Tool::definition_for_*`.** This keeps a special-purpose hook on every tool and still lacks tool-config/MCP context.
- **A second `mcp_skill` tool.** This gives the model two overlapping tools.
- **Store the bound tool once in `SessionRuntime`.** The tool config changes per turn (`SetAllowedTools` etc.), so binding per turn avoids keeping a mutable copy in sync.

### 2. `ToolSet`: session tools first, then the agent registry

A small `ToolSet` (in `tools/`) combines:
- the agent's `Arc<ToolRegistry>`;
- a turn-scoped `ToolRegistry` of session tools. Today that is only the bound `SkillTool`, present when the agent has a catalog.

It exposes `find`, `definitions`, `definition`, `file_access_batch` and `execution_class`. Session tools shadow agent tools of the same name. `ExecutionContext::tool_set(&AgentConfig)` builds it lazily and caches it in a `OnceCell` for the turn.

Every current `config.tool_registry` lookup on the turn path goes through it:
- `collect_tools` (`agent_config.rs`);
- `resolve_tool_schema`;
- prepare, call and truncation hint in `tool_calls.rs`;
- execution class and file access in `transitions.rs`.

Then `Tool::definition_for_cwd`, `ToolRegistry::definitions_for_cwd` and `ToolRegistry::definition_for_cwd` are deleted. `AgentConfig::tool_registry` stays for agent-level tools.

The other `tool_registry` consumers keep the agent registry, deliberately:

| Consumer | Why it stays agent-level |
|---|---|
| `AgentHandle::tool_registry()` (`handle/core.rs`), used by remote/dashboard listings | It describes the agent, not a session. Session-accurate listings already come from `ToolsAvailable` events, which are built by `collect_tools` and therefore include `skill`. Its doc comment states this. |
| `ParallelEditGuard` / `OutputLimit` `bind_agent` | They classify edit tools and output limits. `skill` is read-only and `ParallelSafe`, so classification is unchanged; turn-path file access already goes through `ToolSet`. |
| `delegation/core.rs`, `VerificationService` | They run verification tools, never `skill`. |
| `quorum.rs` placeholder registry, `worktree/service.rs` | Empty or unrelated registries. |

A test asserts that `skill` appears in `ToolsAvailable` for an agent with a catalog even though it is absent from `AgentHandle::tool_registry()`.

Alternative considered: build a full per-session copy of the registry. That clones every builtin each turn for no benefit.

### 3. MCP skills in `McpToolSnapshot`

`McpToolSnapshot` gains `skills: Vec<Arc<McpSkill>>`. `McpSkill` holds:
- `server_name`;
- the validated `rmcp::model::Skill`, with its description already sanitized (§5);
- the server's `Peer<RoleClient>`.

**Eligibility.** A server contributes skills only when it declares the extension (`supports_skills()`) and its name contains no `.`. A server with a `.` in its name is skipped with one warning naming it, because `server.name` IDs, `server.*` selectors and permission prefixes would be ambiguous. Its tools are unaffected. A listed skill is kept only when `Skill::validate` passes and its frontmatter name contains no `.`. Otherwise it is dropped with a warning naming the server and the URI.

How the snapshot is filled:
- **Connect:** `build_mcp_state` and `merge_connected_mcp_peers` call `list_all_skills` for eligible servers, then filter as above. Listing errors are logged with the server name and never fail the connection.
- **Refresh:** a helper `McpToolState::replace_server(server, tools: Option<…>, skills: Option<…>)` replaces one server's slice. It is called only after the network fetch is finished, and applies the change with `ArcSwap::rcu`. The closure is cheap and side-effect free: it rebuilds the maps from the current snapshot, so a concurrent tools refresh and skills refresh for the same or different servers cannot lose each other's update. It always resets `tools_hash` to `None`. Callers:
  - `on_tool_list_changed` passes `tools: Some(..)`, `skills: None` (keeps the server's skills);
  - a new `on_resource_list_changed` passes `tools: None`, `skills: Some(..)`, and does nothing for ineligible servers;
  - a failed listing calls nothing, so the old slice stays.

  This replaces the hand-written retain logic now in `on_tool_list_changed`.
- **Refresh on miss:** when a load asks for `<server>.<name>` and `<server>` is a connected, eligible server whose current slice lacks `<name>`, the tool lists that server's skills once, applies the result through `replace_server`, and retries the lookup. This mirrors the local refresh-on-miss and covers servers whose change notifications never reach the client. It never runs for listing, so schema generation stays network-free.
- **Notification delivery under 2026-07-28.** The design relies on `ClientHandler::on_resource_list_changed`, which the existing harness test shows works for unsolicited list-changed notifications. Task 3.3 checks this against rmcp 3.9's 2026-07-28 behaviour for `resources/list_changed`. If a 2026-07-28 server only emits it on a `listen` subscription, the connect path opens `Peer::listen` with a resources-list-changed filter for each eligible server and forwards each notification to the same refresh code. The spec requirement is phrased by observable behaviour, so either path satisfies it.
  - **Outcome (task 3.3):** confirmed with rmcp 3.10 (`>= 3.9`) in the harness at protocol 2026-07-28 (`modern: true`): an unsolicited `resources/list_changed` from the server reaches `ClientHandler::on_resource_list_changed`, so no `Peer::listen` subscription is opened.

The skill tool's schema is part of the collected tools, so the existing tools hash already detects skill changes and emits tools-available events. No extra change detection is needed.

Alternative considered: a separate skills `ArcSwap` per session. That needs a second refresh path and a second consistency story.

### 4. Callable IDs, visibility and listing

- MCP skills are listed as `<server>.<frontmatter name>`. Because neither part may contain `.` (§3), the first `.` separates the two parts unambiguously.
- The description lists local skills (as today) plus visible MCP skills. The `name` parameter has no schema `enum`: argument validation runs before `call()`, so an enum built from the listing snapshot would reject a skill added since (or one on a server not yet re-listed) before refresh-on-miss could run. Unknown names reach `call()` and fail with the not-found error naming the available IDs. MCP entries say `(MCP server: <server>)`.
- **Collisions:** when a local effective ID equals an MCP ID, the local skill wins and the MCP entry is dropped. A warning naming the local skill path and the server is logged once per pair, tracked in the catalog.
- **Visibility:** a new helper `is_mcp_server_visible(config: &ToolConfig, server) -> bool` in `agent/tools.rs` returns true only when all of these hold:
  - the policy is not `BuiltInOnly`;
  - the denylist contains neither `server.*` nor the bare `server`;
  - the allowlist, when set, contains `server.*`.

  An MCP skill is visible when `catalog.mcp` is on, the `skill` tool itself is admitted by `is_tool_allowed_with`, and `is_mcp_server_visible` holds. Under `ProviderOnly` the builtin `skill` tool is not offered at all, so nothing is visible or loadable. `is_mcp_tool_allowed_with` is not reused because its denylist check would test the literal `*`.
- Lookups at call time use the live `McpToolState` with the same filter, so removed or hidden skills fail with the existing "unavailable, available IDs: …" error. Refresh-on-miss (§3) runs only for visible servers.

### 5. Loading, verification and untrusted content

Limits (constants in `skills/`):
- `MCP_SKILL_DESCRIPTION_MAX = 1024` characters, matching the Agent Skills description limit;
- `MCP_SKILL_MAX_BYTES = 256 KiB` for `SKILL.md` and for each supporting file.

**Listing text.** Server descriptions are sanitized when the snapshot is built: control characters are removed, whitespace runs (including newlines) collapse to one space, and the result is truncated to `MCP_SKILL_DESCRIPTION_MAX` with an ellipsis. The callable ID is built only from validated names. Listing needs no approval, so this is the only server text that reaches the model unapproved.

**MCP skill load** (after visibility and permission checks, §6):
1. `get_skill(uri)` to get the current entry. Run `Skill::validate` on it and require its URI and frontmatter name to equal the listed entry's. Otherwise fail as an integrity error.
2. If resources are `Files`: the manifest must contain a `SKILL.md` entry, otherwise reject. If that entry's declared `size` exceeds `MCP_SKILL_MAX_BYTES`, reject without reading.
3. `read_resource(uri)`. Use exactly the content whose `uri` equals the skill URI; no matching content is an error, and other contents are ignored. Accept text (UTF-8 bytes) or a base64 blob that decodes to UTF-8.
4. Reject if the bytes exceed `MCP_SKILL_MAX_BYTES` (this is the only size bound for `Dynamic`). For `Files`, verify with `SkillResource::verify` (size and `sha256`).
5. Parse with a new string entry point split out of `skills/parser.rs`, in Agent Skills mode rather than `.agents` protocol mode.
6. Require `frontmatter.name == listed name`.

Every failure names the skill and returns no content.

**Output.** It reuses the `<skill_content>` wrapper:
- `<source>mcp:<server></source>` replaces `<base_path>`;
- `<files>` lists the manifest's **relative file keys**, exactly the values the `file` argument accepts. `Dynamic` skills show a note that their files are not listed and `file` reads are unverified.
- Every server-supplied string placed in the wrapper (description, body, file keys, file content) is escaped so that the literal tags `<skill_content` and `</skill_content>` cannot appear unescaped: `<` in those two sequences becomes `&lt;`. The same escaping applies to local skill output, which costs nothing and keeps one code path.
- Body and file output pass through the existing tool output truncation.

**`file` argument:** a skill-relative path. Visibility and permissions are checked first and use the skill's ID. The path is normalized the same way for every skill kind. These forms are rejected before any read or request:
- empty, absolute (`/…`, a drive letter) or scheme-qualified (`x:`) paths;
- a query (`?`) or fragment (`#`);
- backslashes;
- a `.` or `..` segment, also after percent-decoding (`%2e%2e`, `%2E.`);
- an encoded separator (`%2f`, `%5c`).

Then, by skill kind:
- **Local skill:** join the path to the skill directory, canonicalize it, and require it to stay under the canonical skill directory, which rejects symlink escapes. Read as UTF-8 text, bounded by `MCP_SKILL_MAX_BYTES`, with the existing truncation applying.
- **MCP skill with `Files`:** the normalized path must equal a manifest key. Otherwise fail, naming the skill and file, without a request. Check the entry's declared size against the limit, read the URI resolved against the skill root (as rmcp's manifest boundary rules do), select the content with exactly that URI, then verify.
- **MCP skill with `Dynamic`:** resolve the normalized path under the skill root URI, read it, select the exact URI and enforce the byte limit. There is no digest verification.

Alternative considered: a separate resource-read tool. It would expose arbitrary server resources and duplicate permissions.

### 6. Permissions

One deterministic resolver replaces the `HashMap` iteration in `SkillPermissions::check`:
1. an exact entry;
2. else the matching prefix wildcard (`prefix*`, other than `*`) with the longest prefix. Ties cannot occur because keys are unique.

Then:
- **Local skills** (`check`): fall back to `"*"`, else `Allow`. This only changes behaviour where two wildcards overlap, which was non-deterministic before.
- **MCP skills** (`check_mcp(server, name)`), matching against `server.name`: if steps 1–2 found nothing, `"*" = "deny"` denies. A catch-all `allow` or `ask` does not apply. The default is `Ask`. A user who locked down all skills with `"*" = "deny"` therefore also locks down MCP skills, while `"*" = "allow"` cannot silently admit untrusted server content.

**Approval prompt.**
- The prompt names the server and the skill.
- Its elicitation ID is `skill-permission-<id>-<uuid>`. The pending-approval map is shared by all sessions of an agent and provider tool call IDs are not unique across sessions, so a call-ID suffix could replace another session's waiter; a fresh UUID cannot. The local-miss fallback in `call()` applies the same MCP authorization when the name is served by a visible MCP server, so a server registered while the local refresh was pending is never loaded without approval.
- Permissions do not apply to listing, matching today's local behaviour.

**Non-interactive catalogs** (§7): any `Ask` result, for a local or MCP skill, fails immediately with `PermissionDenied`. The message names the skill and the pattern that would allow it (for example `"docs.*" = "allow"` under `[agent.skills.permissions]`). No prompt is sent.

### 7. Sub-agents

**Protocol targets (`.agents` delegation targets):**
- `DotagentsTargetFactory` gains an optional `SkillsConfig` with the registering runtime's skills configuration and that runtime's project root:
  - `api/agent.rs` passes the standalone config, cloned before it is consumed, and the standalone project root;
  - `api/quorum.rs` passes `planner_config.skills` and the quorum's project root.
- `create` attaches `SkillCatalog::from_config(skills, registering_project_root)` under three conditions: skills are enabled, the target is materialized, and the plan's `tools` is absent or contains `skill`. A target session with a cwd uses that cwd at bind time; the project root is only the fallback.
- The catalog is `NonInteractive`. Target sessions own a separate `AgentConfig` and event sink with no fanout to the parent, so an approval prompt would never reach the user and the delegated turn would hang until cancelled. Failing fast with an error that names the pattern to configure is the predictable behaviour; forwarding prompts is a non-goal.
- The target's own sessions bind the tool to their own workspace and their own MCP state, so only the target's attached servers contribute skills. Nothing needs to be copied from the parent.
- Tool policy for targets with `tools` comes from a single shared helper `infer_restricted_tool_policy(tools, mcp_servers) -> ToolPolicy` in `agent/tools.rs`. It returns `BuiltInAndProvider` when servers are attached or any entry contains `.` or equals an attached server's name; otherwise `BuiltInOnly`. `api/agent.rs`, both quorum builders and `DotagentsTargetFactory` all call it, replacing the three existing copies. For quorum this adds the bare-server-name case, a superset that only widens the policy where an attached server is named.

**Configured quorum planner and delegates:**
- They replace `tool_registry_mut().add(build_skill_tool(..))` with `with_skill_catalog(..)` under the same gating.
- Their sessions get MCP skills from their own servers automatically.
- The planner and configured delegates are `Interactive`. **Outcome (task 5.4):** an approval raised by a delegate's skill tool call is emitted as `ElicitationRequested` on the shared fanout and is found by `take_pending_elicitation_sender` from the planner handle (it searches the registry's delegates), so it can be answered through the quorum session. Only protocol targets, whose sessions have a separate event sink, are `NonInteractive`.

Because each sub-agent builds its own `AgentConfig`, the recursion guard in `ChildRegistry` is unaffected.

### 8. Configuration

`SkillsConfig.mcp: bool`, with `#[serde(default = "default_true")]`. It is documented in `configuration.md` next to the permission examples (`"docs.*" = "allow"`), together with the `"*"` rule for MCP skills and the non-interactive behaviour of `.agents` targets.

## Risks / Trade-offs

- **[Breaking Rust API]** Removing `definition_for_cwd` and changing the `SkillTool` constructors breaks external `Tool` implementors that overrode it. Only the skill tool did in-tree. → Called out in the proposal; no compatibility shim, per repo policy.
- **[Extra round trips per load]** `skills/get` plus `resources/read` on every MCP skill load, plus one `skills/list` on a miss. → Loads are rare and user-visible. Caching is a non-goal and can be added later using `ttl_ms`.
- **[Approval fatigue]** `Ask` by default prompts once per load. → Users configure `server.*`. The default is deliberate because server content is untrusted instructions.
- **[Sub-agents need explicit permissions]** Targets cannot prompt, so MCP skills in targets work only with an explicit `allow` pattern. → The error names the exact pattern to add.
- **[Schema churn]** A server that changes skills often changes the tools hash and the prompt cache. → Refreshes happen only on server announcements or load misses, as with tools.
- **[Turn-scoped binding]** Session tools are rebuilt per turn. → Construction is a few `Arc` clones, cached in the `ExecutionContext`.
- **[Name/ID divergence]** A server may list a name that differs from the URI path segment. → The callable ID uses the frontmatter name, and `Skill::validate` (on list and on get) enforces the spec's consistency rules.
- **[Skipped servers]** Servers whose names contain `.` contribute no skills. → A warning names the server; renaming it in configuration fixes it.
- **[Stalled discovery]** `skills/list` is bounded by `MCP_SKILL_LIST_TIMEOUT` (10 s); a timeout logs a warning and the server contributes no skills, so session creation cannot hang on it.
- **[Notification delivery]** If neither handler callbacks nor `listen` deliver resource list changes for some server, the schema lags until a load miss triggers a refresh. → Refresh-on-miss bounds the impact to newly added skills not appearing in the description.
- **[Permission precedence]** Longest-prefix resolution changes which wildcard wins when local patterns overlap. → The previous result was arbitrary, so no configuration could rely on it.

## Migration Plan

No data migration. Rollback is reverting the change. Existing TOML files load unchanged.
- Users who want MCP skills without prompts add `"<server>.*" = "allow"` to `[agent.skills.permissions]`. This is also required for MCP skills in `.agents` delegation targets.
- Users who set `"*" = "deny"` keep MCP skills denied.
- `mcp = false` disables MCP skills.
