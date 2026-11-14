# Proposal

## Why

The 7.3 manual check of `support-mcp-skills` (qmtcode against mcpkit `mcpskills`) found four problems with how skill approvals behave and how they are recorded:

- Every skill load and every supporting-file read prompts again, even for a skill the user has just approved. One five-prompt session showed this. Tool approvals already offer "Allow for this session"; skill approvals do not.
- A `file` path that can never be valid (for example `../secrets.env`) prompts for approval and only then fails.
- The log line `Skill registry refreshed …: 0 skills available` counts only local skills, even when the session is offering MCP skills.
- The session journal records that a question or approval was asked (`elicitation_requested`) but not how it was answered. Nothing records who allowed an untrusted server skill to load.

## What Changes

- **Session-wide skill approval.** A skill approval prompt offers "Allow", "Allow for this session" and "Deny".
  - "Allow for this session" lets later loads of the same skill ID, and reads of its supporting files, run without a prompt in that session only.
  - It applies to MCP skills and to local skills configured as `ask`.
  - Configured `deny` still wins.
  - Agents that cannot prompt (such as `.agents` delegation targets) are unchanged: they still fail at once.
  - Rejections are never remembered.
- **Path check before the prompt.** An invalid `file` path is rejected before any approval prompt, for local and MCP skills. The manifest check for an unlisted file still happens after approval, because it needs the server.
- **Log wording.** The skill registry refresh log names the count as local skills.
- **Answers are journaled.** Every form question (built-in questions, skill approvals, MCP server elicitations) records a durable outcome event when it ends: answered, declined or cancelled.
  - Built-in questions record the selected option labels.
  - Content of MCP server elicitations is never recorded.
- **BREAKING (public Rust API):** `querymt_agent::events::AgentEventKind` gains a variant, `ElicitationResolved`. The enum is not `#[non_exhaustive]`, so downstream exhaustive matches stop compiling. Typeshare output (TypeScript and Swift) gains the type.

## Non-goals

- No server-wide session grant ("allow every `docs` skill for this session"). Configuring `"docs.*" = "allow"` already covers that.
- No grant that outlives the session or is shared with delegate sessions.
- No change to tool permission prompts or to `PermissionGranted`.
- No mitm or tamper tooling for the manual "MCP supporting file is verified" scenario. It stays covered by automated tests.
- No querymt-desktop changes are required. The approval card renders the extra option from the schema. Showing recorded answers in replay is an optional UI follow-up.

## Capabilities

### New Capabilities
- None.

### Modified Capabilities
- `agent-skills`: adds a session-wide allow for skill approvals and requires invalid `file` paths to be rejected without prompting.
- `session-interactions`: adds a durable record of how each form question ended.

This change builds on requirements that `support-mcp-skills` adds to `agent-skills`. Archive `support-mcp-skills` before this one.

## Impact

- **Package:** `querymt-agent` only. No feature-gated code changes. Downstream: `querymt-mobile-ffi` and `querymt-agent-py` compile against `AgentEventKind`; they need to rebuild, and an exhaustive match would need an arm.
- **Code areas:**
  - skill tool approval and `file` handling (`crates/agent/src/skills/tool.rs`);
  - per-session runtime state (`crates/agent/src/agent/core.rs` `SessionRuntime`);
  - the turn tool set binding (`crates/agent/src/agent/execution_context.rs`);
  - the question relay (`crates/agent/src/agent/execution/tool_calls.rs` `tool_context_for_call`);
  - the MCP elicitation handler (`crates/agent/src/elicitation.rs`);
  - events (`crates/agent/src/events.rs`);
  - the catalog refresh log (`crates/agent/src/skills/catalog.rs`).
- **Wire and storage:**
  - ACP: skill approval elicitations carry a third option. The schema shape is unchanged.
  - Journal: a new durable event kind `elicitation_resolved`. There is no schema migration, but an older binary cannot decode journals containing it (see design).
- **Config:** no new keys.
- **Docs:** `docs/docs/agent/configuration.md` (Skill permissions).
