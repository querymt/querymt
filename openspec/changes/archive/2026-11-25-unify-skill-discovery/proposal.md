# Proposal

## Why

Skill folders that are symbolic links are skipped without any message: the skill tool's folder scan does not follow links, so a project whose `.agents/skills` entries are links to shared skill collections sees none of them, and nothing says why. The same folder is also read a second time by the `.agents` Protocol loader, with a different parser, different link rules and different nesting rules, into a manifest skill list that no runtime code uses. The two readers can disagree, and the stricter one reports a linked skill as a path-escape error.

## What Changes

- Skill discovery follows symbolic links to skill folders, wherever the link target is. A broken link or a link loop is reported as a problem and no longer hides the other skills of that folder.
- **BREAKING** Skill discovery no longer applies `.gitignore` (or other ignore files) to skill folders. A skill folder that is ignored by version control is now offered.
- **BREAKING** A project `.agents/skills` folder is scanned to a configurable depth instead of without limit. New key `nested_depth` in every skills table (`[agent.skills]`, a delegate's `skills` table, the deprecated `[planner.skills]`): integer, default `4`, range `1` to `16`.
- A refresh fails only when a skill source folder itself cannot be read. A single entry that cannot be read is reported as a problem and excluded.
- One skill reader and one skill parser. The `.agents` Protocol loader stops reading and parsing skills on its own and uses the skill tool's reader.
- **BREAKING** The resolved manifest's skill list becomes an inventory of every local skill the skill tool offers, from every skill folder (not only `.agents/skills`), each with the folder it came from and, for a linked skill, the link target. `DotagentsSkill` and `parse_skill_document` are removed.
- **BREAKING** A problem with a skill is a warning diagnostic in the manifest. A malformed skill file, or a skill linked from outside the layer, no longer fails a strict `.agents` resolution.
- Skill problems are recorded per workspace on every refresh (path, source folder, kind, message) and announced to the client as an ACP `notice` update with severity `warning`. The structured list is carried in the notice's `_meta.querymt.skillProblems`. The notice is sent only when a session's problem set changes, and only to clients that advertise notice support.
- A new live-only session event carries the problem list from the runtime to the ACP layer.
- Cleanup with no behavior change: startup discovery uses the same path as later refreshes, the duplicated synchronous and asynchronous refresh bodies are merged, and one function decides whether an agent gets the skill tool.
- The time each skill refresh takes is recorded on the existing tool-collection trace span.

## Non-goals

- Deciding whether the `.agents` Protocol on/off switch, layer switches and root overrides apply to skill discovery. Skill discovery keeps reading `~/.agents/skills` and `<workspace>/.agents/skills` whatever those settings say. See design.md, Open Questions.
- An ACP extension method that returns the skill inventory on request. The inventory is available in the resolved manifest and the Rust API only.
- A "nothing changed" shortcut for skill refreshes. This change adds the measurement; a shortcut is a follow-up if the measurement calls for one.
- Listing skills published by MCP servers in the manifest. They belong to a session, not to a folder.
- Changes to skill permissions, to the approval flow, or to how MCP skills are discovered and verified.
- Changing which links the `.agents` Protocol loader accepts for MCP servers, sub-agents, tasks, memories and prompt files. Path confinement stays as it is for those.
- Unifying the override-by-precedence logic that skills, slash commands and `.agents` layers each implement.
- UI work in any client.

## Capabilities

### New Capabilities

None.

### Modified Capabilities

- `agent-skills`: discovery follows links, ignores version-control ignore rules and limits project `.agents/skills` depth by a new config key; refresh fails only on an unreadable source folder; discovery problems are recorded and announced to the client.
- `dotagents-protocol`: the manifest lists every offered local skill with its origin; skill problems are warnings; skill folders are exempt from the symlink confinement rule.

## Impact

**Cargo packages**

- `querymt-agent-runtime` (features `remote`, `oauth`, `plugin-loaders`, `api`; no feature-gated code changes): `skills` (discovery, catalog, registry, tool), `dotagents` (loader, parsers, merge, manifest), `config` (skills settings), `acp` (notice translation), `agent::execution` (announcing problems), `delegation` (member build).
- `querymt-agent-protocol-types`: one new `AgentEventKind` variant and its payload type.
- `querymt-agent`: the builder's skill gate and its `.agents` integration tests.
- `querymt-agent-session-core`: depends on `querymt-agent-protocol-types`; checked for exhaustive matches on event kinds.
- Downstream, build check only: `querymt-agent-py`, `querymt-mobile-ffi`.

**Public Rust API**

- **BREAKING** `querymt_agent::dotagents`: `DotagentsManifest.skills` changes its value type; `DotagentsSkill` and `parse_skill_document` are removed.
- **BREAKING** `querymt_agent::skills`: `discover_from_source`, `discover_all` and `discover_all_strict` return problems alongside skills; `SkillRegistry::load_from_sources` and `SkillRegistry::reload_from_sources` are removed.
- Added: a skill problem type, and read access to a workspace's skill inventory and problems on `SkillCatalog`.

**TOML/JSON config**

- Added: `nested_depth` in skills tables. Existing files keep loading; the JSON schema gains the key.

**ACP wire shape**

- Added: `_meta.querymt.skillProblems` on `notice` session updates. No change to existing payloads.

**typeshare types**

- Added: one `AgentEventKind` variant, generated for TypeScript and Swift. The event is live-only and is never written to the session journal, so stored sessions are unaffected.

**Not affected**: hook and middleware behavior, provider wire shapes, the Extism plugin contract, the session DB schema, Python bindings.

**Client follow-up**

- `querymt-desktop`: handle the new event kind in the generated types and decide whether to show skill problems from the notice or the event.
- `querymt-gpui`: confirm that ACP notices and their `_meta` reach the session view, then show skill problems. It pins `querymt-agent-session-core` by git revision and needs a bump to see the new event kind.

**Documentation**: `docs/docs/agent/configuration.md` (skills settings), `docs/docs/agent/dotagents.md` and `docs/docs/agent/dotagents_support_matrix.md` (skills, manifest, confinement), `CHANGELOG.md` under Unreleased.
