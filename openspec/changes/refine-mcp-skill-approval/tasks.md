# Tasks

Prerequisite: `support-mcp-skills` is archived (or its `agent-skills` delta is synced) before this change is archived, because these requirements build on it.

Verification commands use the querymt-agent CI feature set. `QMT_UI_DIST` must point at the minimal dashboard fixture from `.github/workflows/test-agent.yml` (`index.html` + `querymt-ui.json` in a directory outside `target/`).

## 1. Skill session grants

- [x] 1.1 Add `skill_grants: Arc<ParkingMutex<HashSet<String>>>` to `SessionRuntime` (`crates/agent/src/agent/core.rs`), initialised empty in `SessionRuntime::new`. Thread it through `SkillCatalog::bind` and `SkillTool::bind` from `ExecutionContext::tool_set`. Test helpers and `.agents` target bindings (`crates/agent/src/api/dotagents_targets.rs`) pass a fresh empty set. Setup task; no scenarios.
- [x] 1.2 In `SkillTool::authorize` (`crates/agent/src/skills/tool.rs`), apply the order from design Decision 1:
  - deny first;
  - then allow;
  - then a session grant passes;
  - then a non-interactive catalog fails;
  - then prompt with "Allow", "Allow for this session" and "Deny", where "Allow for this session" inserts the callable ID.

  Tests in `crates/agent/src/skills/tool.rs` with `RecordingContext` and the MCP harness. Each asserts the number of questions asked and the result:
  - `session_allow_skips_later_prompts_for_the_same_skill` covers agent-skills / Session allow skips later prompts for the same skill
  - `session_allow_covers_the_skills_supporting_files` covers agent-skills / Session allow covers the skill's supporting files
  - `session_allow_does_not_cover_other_skills_of_the_server` covers agent-skills / Session allow does not cover other skills of the server
  - `allow_once_is_not_remembered` covers agent-skills / Allow once is not remembered
  - `skill_denial_is_not_remembered` covers agent-skills / Skill denial is not remembered
  - `session_allow_applies_to_an_ask_configured_local_skill` covers agent-skills / Session allow applies to an ask-configured local skill
  - `configured_deny_overrides_a_session_allow`: grant `harness.a` for the session, then bind with `"harness.*" = "deny"` and assert a permission error with zero questions. It covers agent-skills / Configured deny overrides a session allow.
  - also assert that a `NonInteractive` catalog still fails with the pattern hint even when the grant set is non-empty.
- [x] 1.3 Session isolation through the production path. In `crates/agent/src/agent/execution/tool_calls.rs` tests, next to the existing approval test that answers via `take_pending_elicitation_sender`:
  - run `execute_tool_call` for `skill` `harness.a` in one execution context and answer "Allow for this session";
  - call it again in the same context and assert no `ElicitationRequested` event;
  - call it in a second context with its own `SessionRuntime` and assert a new request.

  Test `session_allow_does_not_reach_another_session` covers agent-skills / Session allow does not reach another session.
- [x] 1.4 Update existing tests that assert the two-option schema or the "Allow"-only flow (for example `unconfigured_mcp_skill_asks_and_names_server_and_skill`, `concurrent_loads_get_distinct_elicitation_ids`) to the three options. Concurrent prompts for one ID must stay independent.

## 2. Path check before approval

- [x] 2.1 In `SkillTool::call`, when `file` is present:
  1. resolve the applicable permission level;
  2. return the permission error on `Deny`;
  3. run `normalize_file_path`;
  4. only then authorize.

  Pass the normalized path on to `load_mcp` and `read_local_file`, so the path is not normalized twice. Apply this to the MCP branch and the fallback branch (design Decision 2). Tests in `crates/agent/src/skills/tool.rs` with the MCP harness, using the existing `untouched` helper to check that no request was sent:
  - `traversal_path_fails_without_an_approval_request` asserts the `InvalidRequest` path error, zero questions and an untouched harness. It covers agent-skills / Traversal path fails without an approval request.
  - `denied_skill_with_an_invalid_path_reports_the_denial` asserts `PermissionDenied` with `"harness.*" = "deny"`. It covers agent-skills / Denied skill with an invalid path reports the denial.
  - `unlisted_file_still_asks_first` asserts one question, then a "not listed in its manifest" error, with no `resources/read`. It covers agent-skills / Unlisted file still asks first.
  - also assert that a local skill with `"review" = "ask"` and `file: "../x"` errors without a question.
- [x] 2.2 Re-run the existing normalization and manifest tests (`normalize_rejects_unsafe_paths`, `advertised_files_are_the_ones_load_mcp_file_accepts`, the task 4.4 tests) to confirm nothing regressed.
  - `cargo nextest run -p querymt-agent -E 'test(/skills::/)'`: 156 passed, 0 failed (default features).

## 3. Refresh log wording

- [x] 3.1 Change the debug log in `SkillCatalog::publish_refresh` (`crates/agent/src/skills/catalog.rs`) to `… {count} local skills available`. No scenario. This is checked manually by re-running qmtcode with `RUST_LOG=querymt_agent::skills=debug` against an MCP skill server and confirming the line says "local skills".

  - Code change only; the manual `RUST_LOG=querymt_agent::skills=debug` check against an MCP skill server was not run (needs an interactive qmtcode session).
  - Manual check done (see 6.6): with `RUST_LOG=querymt_agent::skills=debug` against mcpskills the line reads `Skill registry refreshed for /tmp/qmt-73/r2/ws: 0 local skills available` while the session lists `docs.guide`.
## 4. Journal form question outcomes

- [x] 4.1 Add `AgentEventKind::ElicitationResolved { elicitation_id, session_id, outcome, selection }` with kind name `elicitation_resolved`. It is durable through the default arm of `classify_durability`. Extend the events serde round-trip test in `crates/agent/src/events.rs` with the new variant.
  - Added `ElicitationOutcome` (`accepted`/`declined`/`cancelled`) as a typed enum instead of a `String` (the design allows this); `events::tests` 66 passed, including the new round-trip test and the per-variant tag count (102 -> 103).
- [x] 4.2 In the `tool_context_for_call` relay (`crates/agent/src/agent/execution/tool_calls.rs`), register each request with a relay-owned oneshot. A per-request task:
  1. emits `ElicitationRequested`;
  2. awaits the answer;
  3. forwards it to the original `response_tx`;
  4. emits `ElicitationResolved`.

  Receiver errors (the entry was removed or dropped) are recorded as `cancelled`. The cancellation-race branch that skips `ElicitationRequested` emits no resolution. Tests in `tool_calls.rs`, which subscribe to the fanout and read the journal:
  - `skill_approval_answer_is_recorded` answers "Allow for this session" and asserts the outcome `accepted`, the selection `["Allow for this session"]` and the ordering after `elicitation_requested`. It covers session-interactions / Skill approval answer is recorded.
  - `declined_question_is_recorded` declines a `question` tool call and asserts the outcome `declined`. It covers session-interactions / Declined question is recorded.
  - `cancelled_run_records_the_pending_question_as_cancelled` cancels the execution while a skill approval is pending and asserts the outcome `cancelled`. It covers session-interactions / Cancelled run records the pending question as cancelled.
  - Relay moved into `ElicitationRelay::relay_request`. `ask_question` now takes `&[QuestionOption]` instead of `&[(String, String)]` (typed option, per rust-skills `type-no-stringly`). `mcp_tool_tests` 11 passed.
- [x] 4.3 In `McpClientHandler::create_elicitation` (`crates/agent/src/elicitation.rs`), emit `ElicitationResolved` after `rx.await`, with `selection: None`. A closed channel is recorded as `cancelled`. Test `server_elicitation_content_is_not_recorded` accepts with form content and asserts the event has the outcome `accepted` and that the serialized payload contains none of the content values. It covers session-interactions / Server elicitation content is not recorded.
  - `create_elicitation` delegates to a new `McpClientHandler::elicit` so the test can drive it without an rmcp `RequestContext`.
- [x] 4.4 Make sure ACP translation (`crates/agent/src/acp/shared.rs`) and the WebSocket and stdio bridges ignore `ElicitationResolved` without error. Assert it in an existing translation test: the event produces no notification and no panic.
  - Test `elicitation_resolved_produces_no_acp_notification_or_replay_update` in `acp/shared.rs`. The websocket/stdio bridges only match `ElicitationRequested` and need no change.
- [x] 4.5 Before implementing 4.1, resolve the design open question on local downgrade. If downgrade must keep working, stop and revise the design. Otherwise add a CHANGELOG entry that names the new durable event kind and the `AgentEventKind` API break.

  - Decision: local downgrade is not supported (answered by the user). CHANGELOG entry added under "Unreleased".
## 5. Docs

- [x] 5.1 Update `docs/docs/agent/configuration.md` (Skill permissions):
  - the prompt offers "Allow", "Allow for this session" and "Deny";
  - the grant is per skill ID and per session, covers supporting files and never applies to delegates;
  - `deny` always wins;
  - invalid `file` paths fail without a prompt.
- [x] 5.2 Mention `elicitation_resolved` wherever the docs list interaction events (`docs/docs/agent/mesh.md` interaction lifecycle).

## 6. Verification

- [x] 6.1 `cargo check -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote`
  - `QMT_UI_DIST=/tmp/qmt-dashboard-ui` (minimal fixture as in test-agent.yml). `cargo check -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote`: passed. The zbus/systemd features only take effect on Linux; those paths are unverified on macOS.
- [x] 6.2 Run the focused tests:
  ```
  cargo nextest run -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote -E 'test(/session_allow|configured_deny_overrides_a_session_allow|allow_once_is_not_remembered|skill_denial_is_not_remembered|traversal_path_fails_without_an_approval_request|denied_skill_with_an_invalid_path|unlisted_file_still_asks_first|is_recorded|cancelled_run_records|server_elicitation_content_is_not_recorded/)'
  ```
  Then run the skills and approval suites:
  ```
  cargo nextest run -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote -E 'test(/skill|elicitation|approval|permission/)'
  ```
  - Focused filter: 22 passed, 0 failed. `test(/skill|elicitation|approval|permission/)`: 330 passed, 0 failed. Same CI features; nextest 0.9.136.
- [x] 6.3 `cargo fmt -p querymt-agent -- --check`
  - `cargo fmt -p querymt-agent -- --check`: clean.
- [x] 6.4 `cargo clippy -p querymt-agent --all-targets --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote -- -D warnings`. `support-mcp-skills` task 7.1 notes that this was already failing on HEAD. Report pre-existing failures separately from new ones.
  - `cargo clippy -p querymt-agent --all-targets --features <CI set> -- -D warnings`: exit 0, no warnings or errors (no pre-existing failures observed on this tree).
- [x] 6.5 Downstream: `cargo check -p querymt-mobile-ffi` and `cargo check -p querymt-agent-py`, with `PYO3_PYTHON` set to Python ≥ 3.10.
  - `cargo check -p querymt-mobile-ffi`: passed. `PYO3_PYTHON=/opt/homebrew/bin/python3.14 cargo check -p querymt-agent-py`: passed.
- [x] 6.6 Manual re-check with the 7.3 fixture (`/tmp/qmt-73`, mcpkit `mcpskills`):
  - choose "Allow for this session" for `docs.guide` and confirm that a second load and a `templates/checklist.md` read do not prompt;
  - confirm `../secrets.env` fails with no prompt;
  - confirm the session journal has an `elicitation_resolved` event after each `elicitation_requested`.

  - Result (live, qmtcode from this branch + mcpskills v0.8.21, `claude-sonnet-5-5` over OAuth, answers sent with qmacp `--elicitation`; fixture `/tmp/qmt-73/r2`):
    - Prompt shows three options (Allow / Allow for this session / Deny).
    - "Allow for this session" (session `01a1d5dd-e94c-73c6-a036-796a820a7228`): six loads of `docs.guide` plus a `templates/checklist.md` read produced exactly one `elicitation`. A new session prompted again (`01a1d5de-5a18-7518-ba2b-18b76abb0b1b`).
    - "Allow" (`01a1d5dd-bcb3-70f3-a8c4-adc6ea6fafc4`) and "Deny" (`01a1d5de-6201-7716-82d5-4d6c2bdcc2ce`) were not remembered (a prompt per load); decline is recorded as `declined`.
    - `file=../secrets.env` (`01a1d5d7-e441-77fe-8029-9124c2128a81`): `Invalid request: invalid file path ... '.' and '..' segments are not allowed`, zero elicitations, no request to the server.
    - Journal: every `elicitation_requested` has a matching `elicitation_resolved` after it (`cancelled`, `accepted` with the selected label, `declined`); no ordering violations; payload carries no form content.
    - Also confirmed: `"docs.*" = "allow"` no prompt; `"*" = "deny"` blocked without prompt; `"*" = "allow"` still prompts; `mcp = false` hides and rejects the skill.
- [x] 6.7 `openspec validate refine-mcp-skill-approval --strict`

  - `openspec validate refine-mcp-skill-approval --strict`: valid.