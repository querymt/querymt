# Tasks

Conventions used below. They are reference text, not tracked tasks.

- This change starts after `split-agent-crate` has merged into `main`, on a branch from `main`. Paths are relative to `crates/`: "runtime" is `querymt-agent-runtime` (`agent-runtime/src`), "services" is `querymt-agent-services` (`agent-services/src`), "top" is `querymt-agent` (`agent/src`).
- Features: `F=dashboard,oauth,zbus-secret-service,systemd-credentials,remote` (top, the CI set in `.github/workflows/test-agent.yml`) and `RF=remote,oauth,plugin-loaders,api` (runtime, `runtime_features` in the same workflow). The zbus and systemd features only take effect on Linux; on macOS those paths stay unverified.
- `QMT_UI_DIST` points at the fixture created in 1.1. `QUERYMT_REMOTE_CONNECT_OPEN_BUDGET_MS` is unset for every run (it makes `connect_budget_defaults_are_bounded_and_reason_specific` fail).
- "Run-R `<filter>`" means `cargo nextest run -p querymt-agent-runtime --features "$RF" -E 'test(/<filter>/)'`. "Run-T `<filter>`" means `cargo nextest run -p querymt-agent --features "$F" -E 'test(/<filter>/)'`. A runtime edit costs about 70 s per test build, so run the narrowest package first.
- A group gate runs, in order:
  1. `cargo check -p querymt-agent-runtime --all-targets --features "$RF"` and `cargo check -p querymt-agent-runtime --no-default-features`;
  2. `cargo check -p querymt-agent --all-targets --features "$F"` and `cargo check -p querymt-agent --lib --no-default-features`;
  3. `cargo check -p querymt-mobile-ffi`;
  4. the group's filters with Run-R and Run-T;
  5. `cargo fmt -p querymt-agent-runtime -p querymt-agent -- --check`;
  6. `cargo clippy -p querymt-agent-runtime --all-targets --features "$RF" -- -D warnings` and `cargo clippy -p querymt-agent --all-targets --features "$F" -- -D warnings`.

  Groups that touch services also run `cargo check -p querymt-agent-services --no-default-features`, `--features test-support`, and `cargo clippy -p querymt-agent-services --all-targets -- -D warnings`.
- Scenario tests are named after the scenario in snake_case. Where a test lives:
  - Runtime, by default: tests that need only a `LocalAgentHandle` built by `delegation::build_root`, a roster or the orchestrator. They use the scripted-provider helpers that 2.1 moves into runtime `test_utils`.
  - Top `api/agent_tests.rs` or `runtime_tests/`: only tests that need `Agent`, `AgentBuilder`, TOML profiles or the profile manager.

## 1. Baseline

- [x] 1.1 Create the dashboard fixture outside `target/` (`index.html` and `querymt-ui.json` as in `test-agent.yml`), export `QMT_UI_DIST`, and confirm `cargo nextest --version`, `cargo check -p querymt-agent-runtime --features "$RF"` and `cargo check -p querymt-agent --features "$F"` succeed.
  - Done: fixture in `/tmp/qmt-dashboard-ui`, `QMT_UI_DIST=/tmp/qmt-dashboard-ui`; `cargo nextest` 0.9.136; runtime check (`$RF`) and top check (`$F`) pass on macOS (zbus/systemd paths unverified).
- [x] 1.2 Add characterization test `quorum_profiles_keep_their_runtime_shape` in top `api/agent_tests.rs`. For each `[quorum]` profile in `agent/examples/confs` (`coder_agent`, `coder_delegate`, `learning_pair`, `multi_agent`), build it through `Agent::from_quorum_config` and assert:
  - root tool names and member IDs;
  - middleware names per agent;
  - snapshot policy and backend per agent;
  - delegation settings.

  Verify: Run-T `quorum_profiles_keep_their_runtime_shape`; it passes on the unchanged code.
  - Done: Run-T passes (1 passed); Run-R `config::tests` 119 passed; `cargo fmt -p querymt-agent-runtime -p querymt-agent -- --check` clean.
  - Test sets `GITHUB_TOKEN` itself because `multi_agent.toml` interpolates it (safe: nextest runs one test per process).
  - Added read-only `DelegationOrchestrator::config()` (runtime `delegation/core.rs`) so the test can assert delegation settings.
  - Pre-existing example defects found on HEAD and fixed (user-approved) so the profiles load at all: `coder_agent.toml` had `[delegates.parameters]` commented out (duplicate `model` key, invalid TOML) and referenced the missing `../prompts/anthropic_opencode.txt` (now `../prompts/coder.md`); `multi_agent.toml` prompt paths were `prompts/*.md` and are now `../prompts/*.md`. The `config/tests.rs` comment saying `coder_agent.toml` is invalid TOML is now stale; its textual check still passes.
- [x] 1.3 Record the starting point under `openspec/changes/unify-agent-runtime/baseline/` (create the directory). Verify: the four files are committed.
  - Done: five files recorded (the task text says four but lists five): `nextest-list.txt` (3946 tests: runtime `$RF`, top `$F`, services), `line-counts.txt` (agent 35580, agent-core 18248, agent-index 15142, agent-knowledge 26987, agent-runtime 177764, agent-services 65877, agent-sqlite 22313), `types.ts` (2367 lines), `schema-single.json`, `schema-quorum.json`. The list includes the 1.2 test. Schemas were dumped by a temporary test that was removed afterwards.
  - `nextest-list.txt`: the sorted `cargo nextest list --message-format oneline` over `querymt-agent-runtime` (`$RF`), `querymt-agent` (`$F`) and `querymt-agent-services`.
  - `line-counts.txt`: `wc -l` over `crates/agent*/src` per package.
  - `types.ts`: typeshare output of `scripts/generate-types.sh`.
  - `schema-single.json` and `schema-quorum.json`: the `JsonSchema` output of `SingleAgentConfig` and `QuorumConfig`.

## 2. Move the member build path into runtime (no behaviour change)

- [x] 2.1 Move the scripted-provider helpers out of runtime `agent/delegation_loop_tests.rs` (`TestHarness`, `build_delegate_handle`, `agent_info`, `DelegateBehavior` and the middleware stubs they need) into a new runtime `test_utils/delegation.rs`, gated on `cfg(any(test, feature = "test-support"))`. `delegation_loop_tests.rs` keeps its tests and imports the helpers. Verify: Run-R `delegation_loop` and Run-T `delegation` pass; the test list equals the 1.3 list.
  - Done: Run-R `delegation_loop` 28 passed; Run-T `delegation` 4 passed; sorted `cargo nextest list` over runtime/top/services equals `baseline/nextest-list.txt`. `test_utils` is already gated by `cfg(any(test, feature = "test-support"))` in `lib.rs`. `TestHarness.exec_ctx` is `pub(crate)` because `ExecutionContext` is.
- [x] 2.2 Move the member build code (design D0). Verify:
  - `git show --stat` of the move commit shows renames only;
  - both packages' full nextest runs pass and the test list equals the 1.3 list except for the moved tests' paths (name them below this task);
  - `ProtocolTargetFactory` and its only implementation are both in runtime;
  - top no longer calls `AgentConfigBuilder::new` for a delegate or protocol target;
  - the schema and typeshare outputs equal the 1.3 files.

  Result (commands: `$RF`/`$F` as in the conventions):
  - Renames-only commit `refactor: move dotagents target factory into runtime (rename only)` (`git show --stat`: 1 file, 0 insertions, 0 deletions). Glue commit follows it.
  - Full nextest: runtime 2421 passed, 6 skipped; top 519 passed, 1 skipped.
  - Test list vs `baseline/nextest-list.txt`: the only differences are the 7 moved tests, `querymt-agent api::dotagents_targets::tests::*` -> `querymt-agent-runtime dotagents::target_factory::tests::*` (`parent_without_skills_yields_no_catalog`, `quorum_style_target_applies_the_registering_permissions`, `restricted_target_with_mcp_servers_keeps_only_listed_tools`, `restricted_target_without_servers_is_builtin_only`, `restricted_target_without_skill_gets_no_catalog`, `target_loading_an_unconfigured_mcp_skill_fails_at_once`, `unrestricted_target_offers_skills_non_interactively`).
  - typeshare output equals `baseline/types.ts`; no file under runtime `config/` changed, so the schemas equal the baseline.
  - `ProtocolTargetFactory` and its only production implementation (`DotagentsTargetFactory`) are both in runtime.
  - Top `AgentConfigBuilder::new` remains only for the root agent (`api/agent.rs`) and the planner (`api/quorum.rs`); delegates and protocol targets are built by runtime `delegation::member::configure_delegate_builder` and `DotagentsTargetFactory::create_or_bare`.
  - Public items added in runtime for the glue: `delegation::member::{MemberBuildSpec (doc-hidden), DelegateBuildParams, configure_delegate_builder, apply_middleware_from_config, parse_tool_specs, infer_required_capabilities}`, `dotagents::DotagentsTargetFactory::create_or_bare`. `api::DotagentsTargetFactory` is re-exported from runtime.

  Steps:
  1. In a renames-only commit, `git mv agent/src/api/dotagents_targets.rs agent-runtime/src/dotagents/target_factory.rs`.
  2. In the glue commit, add runtime `delegation/member.rs`. It holds `MemberBuildSpec` (top `api::config::AgentConfig`, renamed, `pub` with `#[doc(hidden)]`), `parse_tool_specs`, `infer_required_capabilities` and `FILESYSTEM_TOOLS` (from top `api/utils.rs`). It also holds the body of the delegate closure in top `api/quorum.rs:602-735`, as a function that returns the configured `AgentConfigBuilder`.
  3. Top `api/config.rs` builders produce `MemberBuildSpec`. Top `api/quorum.rs` and `api/agent.rs` call the runtime functions. `api::DotagentsTargetFactory` is re-exported from runtime so its path is unchanged.
- [x] 2.3 Group gate with filters `delegation|dotagents|quorum_profiles|config::tests`.
  - Done: all six gate steps pass. check runtime (`$RF` all-targets, `--no-default-features`), top (`$F` all-targets, `--lib --no-default-features`) and `querymt-mobile-ffi`; Run-R filters 484 passed; Run-T filters 39 passed; fmt clean; clippy `-D warnings` clean for both packages (fixed `new_without_default` on the moved `ContextThresholdOnceMiddleware` and `type_complexity` in the 1.2 test).

## 3. One builder, delegation service in every runtime

- [x] 3.1 Add three failing tests in top `api/agent_tests.rs`. Verify: the first two fail on their timeout with the delegation still `requested`; the third fails with `did not use the quorum event fanout`.
  - Done (Run-T, `$F`): `delegation_from_a_plain_agent_runs` and `internal_sub_agent_extends_a_standalone_agent` fail after the 5 s wait with status still `Requested` (`left != Complete`); `runtime_with_configured_delegates_and_a_protocol_target_starts` fails with `Error: writer agent did not use the quorum event fanout`. They are expected to fail until 3.3/3.4. The tests record a `DelegationRequested` event the way the `delegate` tool does, because no provider script is needed to prove the service runs it.
  - `delegation_from_a_plain_agent_runs` (agent-delegation / Delegation from a plain agent runs, with a pre-registered member);
  - `internal_sub_agent_extends_a_standalone_agent` (dotagents-protocol / Internal sub-agent extends a standalone agent, with a `.agents` target);
  - `runtime_with_configured_delegates_and_a_protocol_target_starts` (dotagents-protocol / Runtime with configured delegates and a protocol target starts).
- [ ] 3.2 Move service ownership to the root handle. Runtime `LocalAgentHandle` owns the `DelegationOrchestrator` and its listener and stops them in `shutdown` (`agent/handle/core.rs`). Top `AgentQuorum` calls it. Verify: Run-R `delegation_loop` and Run-T `quorum_parent_clock`; both pass unchanged.
- [ ] 3.3 Add runtime `delegation::build_root` and `RootDelegation` (design D5):
  - The delegation-enabled rule lives in one function.
  - The delegation summarizer and the peer routing (`RoutingActor`) setup move from top `api/quorum.rs:737-1000`.
  - `build_root` registers the tool, starts the service when enabled, and passes the profile ID to the orchestrator.

  Give top `AgentBuilder` `.delegate(..)`, `delegation_settings(..)`, `delegation(bool)` and `with_profile_id` (moved from `QuorumBuilder`), and make it build its root only through `build_root`.

  Tests:
  - `delegation_from_a_plain_agent_runs` and `internal_sub_agent_extends_a_standalone_agent` now pass.
  - Add in runtime `delegation/root_tests.rs`: `unrestricted_agent_can_delegate` and `agent_whose_tools_exclude_delegate_cannot_delegate` (agent-delegation / same names).
  - Add in top: `two_profiles_delegate_at_the_same_time` (agent-delegation / Two profiles delegate at the same time).

  Verify: Run-R `root_tests`, and Run-T on the three top names.
- [ ] 3.4 Collapse the quorum. Verify: Run-T `runtime_with_configured_delegates_and_a_protocol_target_starts|internal_sub_agent_extends_a_quorum_profile|quorum_profiles_keep_their_runtime_shape|parent_clock_keeps_three_shell_delegations`; `cargo check -p querymt-mobile-ffi`.

  Code changes:
  - `Agent::builder_from_quorum_config` returns `AgentBuilder`.
  - Delete `QuorumBuilder`, `QuorumSession`, top `api/quorum.rs`, top `quorum.rs`, `Agent::{multi, quorum, planner}` and the `EventFanoutMismatch` check.
  - Collapse the `match Config` build arms in top `runner.rs`, top `profiles.rs` and `querymt-mobile-ffi/src/agent.rs`, and remove the four `quorum.shutdown()` blocks in `profiles.rs`.
  - `ProfileRuntime::view()` and `Agent::is_multi()` read the root's registry until 4.3.
  - Remove the `quorum` re-exports from top `lib.rs` and `prelude.rs`.
  - Port `quorum_parent_clock_keeps_three_shell_delegations_terminal_on_wire` to `Agent`.

  Tests:
  - `runtime_with_configured_delegates_and_a_protocol_target_starts` now passes.
  - Add `internal_sub_agent_extends_a_quorum_profile` (dotagents-protocol / Internal sub-agent extends a quorum profile).
- [ ] 3.5 Update `docs/docs/agent/delegation.md` (architecture: root, roster, service in every runtime), `docs/docs/agent/api_reference.md` and `docs/docs/agent/examples.md`. Port `agent/examples/closure_builder.rs` and `agent/examples/auto_delegation_example.rs` from `Agent::multi()` to `Agent::single().delegate(..)`. Verify: `cargo check -p querymt-agent --examples --features "$F"` and `cargo test -p querymt-agent --doc --features "$F"`.
- [ ] 3.6 Group gate with filters `delegation|quorum_profiles|sub_agent|two_profiles|root_tests`.

## 4. Typed roster and one member build path

- [ ] 4.1 Add runtime `delegation/roster.rs` with `AgentId`, `RosterGeneration`, `MemberOrigin`, `MemberLocation`, `Member`, `MemberAgent`, `MemberSpec`, `RosterSnapshot`, `AgentRoster` and `RosterError` (design D2 to D4). Unit tests in the file:
  - an empty or blank ID is rejected;
  - the JSON form of `AgentId` is a plain string;
  - add bumps the generation by one;
  - a duplicate ID and an unknown ID return the typed error and leave the snapshot unchanged;
  - iteration is in ID order.

  Verify: Run-R `roster::tests`.
- [ ] 4.2 Make `build_local_member` and `MemberContext` the one build function in runtime `delegation/member.rs`, used by both lowering functions moved in 2.2 (from `DelegateConfig`/`MemberBuildSpec` and from `DotagentsSubAgentPlan`). Test `every_member_shares_the_runtime_event_stream` (runtime) asserts `Arc::ptr_eq` with the root's fanout for a configured and a protocol member. Verify: Run-R `every_member_shares_the_runtime_event_stream`.
- [ ] 4.3 Cut over to the roster. Steps:
  1. Runtime `AgentConfig.agent_registry` becomes `roster: Option<Arc<AgentRoster>>`, and `build_root` builds the roster.
  2. The orchestrator takes `Weak<LocalAgentHandle>`, the roster and `DelegationSettings`, and loses `delegator`, `inject_results` and `inject_failure`.
  3. Delete the `AgentRegistry` trait, `DefaultAgentRegistry`, `DotagentsTargetRegistry`, `LazyTarget`, `ProtocolTargetFactory`, `ChildRegistry`, `AgentConfigBuilder::{with_agent_registry, with_agent_registry_only}`, the `AgentHandle` trait with `agent/handle/agent_handle_impl.rs`, and its `as_any` downcasts in runtime and top.
  4. `RemoteAgentHandle` stays inside `MemberAgent::Remote` until group 6.
  5. `ProfileRuntimeView` loses `delegates`; `delegate`/`session_handle` read the roster and return `Arc<LocalAgentHandle>`. `Agent::delegate` returns `Arc<LocalAgentHandle>`, and `Agent::is_multi` reports whether the roster has members.
  6. Remove `AgentHandle` and `AgentRegistry` from top `lib.rs` and `prelude.rs` and the `api::DotagentsTargetFactory` re-export from top `api/mod.rs`, and add `AgentRoster`, `AgentId` and `MemberSpec` to the prelude.
  7. Port the stub-based orchestrator tests in runtime `delegation/core.rs` to real local members, and port runtime `dotagents/target_registry_tests.rs` and the tests of `dotagents/target_factory.rs`.

  Tests, renamed to the scenario names:
  - `protocol_target_is_built_on_first_delegation`;
  - `listing_does_not_build_members` (agent-roster / Listing does not build members);
  - `explicit_delegate_collides_with_a_protocol_target`, `delegation_is_disabled`, `profile_is_disabled`, `external_connection_cannot_be_represented_safely` (dotagents-protocol / same names);
  - add `protocol_target_that_cannot_be_built_fails_the_delegation` (agent-delegation / same name);
  - add `session_bound_to_a_protocol_member_loads_after_restart` (top; covers the `ProfileRuntimeView` change).

  Verify: Run-R `protocol_target|listing_does_not_build|explicit_delegate_collides|delegation_is_disabled|profile_is_disabled|external_connection|delegation::`, Run-T `session_bound_to_a_protocol_member|profiles::`, and `cargo check -p querymt-mobile-ffi`.
- [ ] 4.4 Move the agent list into the tool (design D6). Verify: Run-R on those four names plus `config::tests`; the services checks from the conventions; `http` is no longer in `agent-services/Cargo.toml`.

  Code changes:
  - `DelegateTool` holds the roster and renders it in `definition()`. `DelegateTool::new` is `pub(crate)` and is registered only by `build_root` on a root with delegation enabled.
  - `AgentConfig::build_delegation_meta` reads the snapshot.
  - `all_builtin_tools()` no longer contains `delegate`. Add `builtin_tool_names()` with `delegate` in its historical place, and switch selector validation, `FILESYSTEM_TOOLS`, `config/tests.rs` (the `AgentSettings.tools` description test) and `builtin_tool_names_unchanged_after_split` to it.
  - Remove the registry parameter of `ExecutionContext::tool_context`.
  - In services, delete `ToolContext::extensions()`, `AgentToolContext::with_extension` and the `extensions` field, and drop `http` from `agent-services/Cargo.toml`.
  - Delete runtime `middleware/delegation.rs`, `DelegationContextConfig`, `DelegationContextTiming` and the `AgentConfigBuilder` methods that set them. Remove their re-exports from top `lib.rs` and `prelude.rs`.

  Tests:
  - `delegate_tool_lists_members_ordered_by_id` and `unchanged_roster_keeps_the_tool_definition_unchanged` (agent-roster / same names);
  - `member_agents_are_not_offered_the_delegate_tool` (agent-delegation / same name);
  - `builtin_tool_names_unchanged_after_split` still passes against the unchanged baseline file.
- [ ] 4.5 Update `docs/docs/agent/delegation.md` (where the agent list appears) and `docs/docs/agent/middleware.md` (remove the delegation middleware). Verify: `grep -rn "DelegationMiddleware\|DelegationContext" docs/docs` returns nothing.
- [ ] 4.6 Group gate, with the services checks, using filters `roster|delegat|protocol_target|member_agents|builtin_tool_names|profiles::`.

## 5. One event stream, interactions reach the user

- [ ] 5.1 Make ACP transports subscribe to the root's `EventFanout` only. Delete `collect_event_sources` (runtime `acp/shared.rs`), and make `event_sources` one `Arc<EventFanout>` in runtime `acp/mod.rs` and `acp/stdio.rs` and in top `acp/websocket.rs`. Switch the registry walks in runtime `elicitation.rs`, `agent/handle/runtime_observation.rs`, `agent/handle/core.rs` and `agent/handle/remote_mesh.rs` to `snapshot.built_local()`.

  Test `protocol_target_activity_reaches_the_parents_client` (runtime; agent-delegation / Protocol target activity reaches the parent's client): subscribe as the transport does, and assert that the child's tool and message events arrive and that `is_event_owned` attributes them to the parent's connection.

  Verify: Run-R `protocol_target_activity_reaches_the_parents_client`; Run-T `websocket`.
- [ ] 5.2 Delete `skills::ApprovalMode` (runtime `skills/catalog.rs`), its branch in runtime `skills/tool.rs`, the call in runtime `dotagents/target_factory.rs` and the tests that construct `NonInteractive`.

  Tests in runtime: `protocol_target_skill_approval_reaches_the_user` and `protocol_target_tool_permission_request_reaches_the_user` (agent-delegation / same names). They use `test_utils::mcp_harness::HttpHarness` as `target_sees_only_its_server_skills_and_parent_sees_both` (top `api/agent_tests.rs`) does.

  Verify: Run-R on those two names and `skills::`; Run-T `target_sees_only_its_server_skills`.
- [ ] 5.3 Bring the in-flight changes in line:
  - In `openspec/changes/support-mcp-skills`, remove the requirement "Protocol targets do not wait for skill approval" from `specs/dotagents-protocol/spec.md`, rewrite the `NonInteractive` paragraphs of design §7, and update the tasks that reference them.
  - In `openspec/changes/refine-mcp-skill-approval`, remove the `NonInteractive` step from `design.md` and its tasks.

  Verify: `openspec validate support-mcp-skills --type change --strict --no-interactive` and `openspec validate refine-mcp-skill-approval --strict` pass, and `grep -rn "NonInteractive" openspec/changes crates/agent*/src` returns nothing.
- [ ] 5.4 Group gate with filters `reaches_the|skills::|elicitation|websocket`.
- [ ] 5.5 Live check with the fixture in `/tmp/qmt-73/r2` and `qmacp --elicitation`. Use a separate `qmtcode` (`cargo build -p querymt-agent --example qmtcode --features "$F"`) on port 3031, never the user's instance.
  - (a) The standalone config delegating to `writer` with no skill permission pattern shows one approval prompt in the parent's stream; `accept=Allow` loads `docs.guide`, and the delegation completes.
  - (b) The same with `"docs.*" = "allow"` shows no prompt.
  - (c) The `[quorum]` fixture with the `.agents` target starts.

  Verify: record session IDs and journal rows in `openspec/changes/support-mcp-skills/tasks.md` 7.3, and tick it only if all three pass.

## 6. Remote members

- [ ] 6.1 Add remote members, all in runtime under the `remote` feature:
  - Add `RemoteMember` and the root-handle method that opens a delegated session through `create_remote_session` and `connect_remote_session` with `RemoteConnectReason::Delegation`.
  - Build remote members from `[[remote_agents]]` without contacting the peer.
  - Delete `agent/remote/remote_handle.rs`, `agent/remote/remote_agent_stub_tests.rs`, `RemoteAgentHandleFixture` (`agent/remote/test_helpers.rs`), and `register_remote_agents_from_config` / `register_remote_agent` (`agent/remote/remote_setup.rs`).

  Tests with the two-node fixture of runtime `agent/remote/test_helpers.rs`:
  - `remote_member_is_listed_while_its_peer_is_unreachable`, `delegation_succeeds_once_the_peer_appears` (agent-delegation / same names);
  - `members_from_every_source_are_listed_together` (agent-roster / same name).

  Verify: Run-R on those three names.
- [ ] 6.2 Tests `remote_member_activity_reaches_the_parents_client` and `remote_member_interaction_reaches_the_user` (runtime, two-node fixture; agent-delegation / Remote member activity reaches the parent's client, Remote member interaction reaches the user). Verify: Run-R on those two names alone, since mesh timing tests can flake in a shared run.
- [ ] 6.3 Check that delegated remote sessions do not show up as user sessions. In `delegated_remote_session_is_not_listed_as_a_user_session` (runtime), assert that `list_remote_sessions` and startup reattachment skip sessions whose parent is a delegation. If the test fails, filter those sessions. Verify: Run-R on that name.
- [ ] 6.4 Update `docs/docs/agent/mesh.md` ("Remote Agents": resolved on first delegation, no startup wait) and the "Remote Delegation" section of `docs/docs/agent/delegation.md`. Verify: the documented TOML loads in a parse test `mesh_docs_remote_agent_example_loads` in runtime `config/tests.rs`.
- [ ] 6.5 Group gate with filters `remote_member|delegated_remote|peer_appears|every_source|mesh_docs`. The gate's `--no-default-features` checks confirm the feature gate for runtime and top.

## 7. Changing the roster while running

- [ ] 7.1 Add the runtime `LocalAgentHandle` methods that change the roster, including the `querymt/agents/changed` broadcast and the rejections of design D9. Add top `Agent::roster`, `Agent::add_member` and `Agent::remove_member`, which call them.

  Tests in runtime:
  - `member_added_to_a_plain_agent_can_be_delegated_to`, `removed_member_is_no_longer_a_delegation_target`, `running_delegation_survives_removal_of_its_member`, `root_agent_is_told_about_a_member_added_between_turns`, `root_agent_is_told_when_a_member_was_removed` (agent-roster / same names);
  - `activity_of_a_member_added_after_connect_reaches_the_client` (agent-delegation / same name).

  Test in top: `agent_roster_methods_forward_to_the_root`.

  Verify: Run-R on the six names; Run-T `agent_roster_methods_forward_to_the_root`.
- [ ] 7.2 Add `querymt/agents/list`, `querymt/agents/add` and `querymt/agents/remove` in a new runtime `agent/handle/ext_roster.rs`, dispatched from `agent/handle/ext.rs`, with optional `profileId`. Advertise them and the notification in runtime `control/capabilities.rs`. Verify: Run-R `roster_ext` and Run-T `roster_ext`.

  Tests in runtime `agent/handle/roster_ext_tests.rs`, all agent-roster / same names:
  - `plain_agent_has_an_empty_roster`, `roster_methods_are_advertised`;
  - `added_remote_member_is_listed`, `remote_member_without_a_mesh_is_rejected`;
  - `duplicate_member_id_is_rejected`, `invalid_member_is_rejected`, `peer_routing_is_not_accepted_for_a_member_added_at_runtime`, `adding_a_member_while_delegation_is_disabled_is_rejected`;
  - `removing_an_unknown_member_fails`, `clients_are_notified_of_a_roster_change`.

  Tests in top `runtime_tests/handle/roster_ext.rs`, because they need the profile manager or a rebuild from configuration: `runtime_changes_are_gone_after_restart`, `adding_a_member_to_one_profile_leaves_another_unchanged`, `unknown_profile_is_rejected`.
- [ ] 7.3 Document the three methods, the notification, the member JSON shape and "not persisted" in `docs/docs/agent/delegation.md` under a new section "Changing agents while running", next to the existing ACP delegate-model section. Verify: every JSON example in the section is parsed by test `roster_docs_examples_parse` (runtime).
- [ ] 7.4 Group gate with filters `roster|member_added|removed_member|running_delegation_survives|root_agent_is_told`.

## 8. Configuration

- [ ] 8.1 Move `DelegateConfig` from runtime `config/quorum.rs` to a new `config/delegates.rs` with `DelegationSettings`; the `config::*` re-exports keep both paths. Add `delegates` to `SingleAgentConfig` and `[agent.delegation]` (`DelegationSettings`) to `AgentSettings` (`config/single.rs`), with JsonSchema entries. Add load-time validation in `config/loader.rs`: duplicate ID, isolated delegate without `[agent.workspace]`, `max_parallel >= 1`.

  Tests in runtime `config/tests.rs` (agent-configuration / same names):
  - `agent_configuration_with_delegates_loads`, `unknown_delegate_key_is_rejected`, `duplicate_delegate_id_is_rejected`, `isolated_delegate_without_the_workspace_service_is_rejected`;
  - `delegation_settings_default_when_omitted`, `delegation_settings_accept_valid_values`, `invalid_delegation_settings_are_rejected`, `zero_parallel_delegations_is_rejected`.

  Verify: Run-R `config::tests`. The `QuorumConfig` schema equals `baseline/schema-quorum.json`, and the `SingleAgentConfig` schema differs from `baseline/schema-single.json` only by the two additions.
- [ ] 8.2 Deprecate `[quorum]`: log one notice per load naming `[agent]`, `[[delegates]]` and `[agent.delegation]`, and reject a file with both `[agent]` and `[quorum]` or `[planner]`. Tests in runtime: `quorum_configuration_loads_with_a_deprecation_notice`, `agent_and_quorum_tables_cannot_be_mixed` (agent-configuration / same names). Verify: Run-R on those two names.
- [ ] 8.3 Rewrite the four `[quorum]` profiles in `agent/examples/confs` as `[agent]` with `[[delegates]]` and `[agent.delegation]`, following the table in design D10. Extend `quorum_profiles_keep_their_runtime_shape` to also load the rewritten files and assert the same shape as the recorded baseline. Verify: Run-T `quorum_profiles_keep_their_runtime_shape|profiles::`, and Run-R `config::tests` (it reads `../agent/examples/confs`).
- [ ] 8.4 Update `docs/docs/agent/configuration.md`: new keys with type, default and limits, and the "Multi-Agent (Quorum) Configuration" section becomes a migration section with the D10 table. Update the `[quorum]` examples in `profiles.md`, `agent_modes.md`, `worktrees.md` and `middleware.md`. Verify: `grep -rn "^\[quorum\]\|^\[planner\]" docs/docs crates/agent/examples/confs` returns only the migration section.
- [ ] 8.5 Group gate with filters `config::tests|quorum_|profiles::`.

## 9. Downstream packages and integration

- [ ] 9.1 `querymt-agent-py`: `is_multi()` already reports whether the roster has members (4.3). Update its docstring and replace any removed API. Verify: with `PYO3_PYTHON` set to Python 3.10 or newer, run `cargo check -p querymt-agent-py`, then `cargo fmt -p querymt-agent-py -- --check` and `cargo clippy -p querymt-agent-py --all-targets -- -D warnings`. If `pyo3-ffi` cannot build here (as in `split-agent-crate` 4.1), record that and leave this task to CI.
- [ ] 9.2 `querymt-mobile-ffi`: both config shapes go through the one builder call (done in 3.4). Remove the now-dead quorum log text. Verify: `cargo check -p querymt-mobile-ffi`, `cargo nextest run -p querymt-mobile-ffi`, then fmt and clippy for the package.
- [ ] 9.3 Port `agent/benches/agent_startup.rs` and the remaining examples to the new API. Verify: `cargo check -p querymt-agent --benches --examples --features "$F"`.
- [ ] 9.4 Full run and measurement. Record the numbers below this task; the timing is not a pass/fail gate.
  - Run `cargo nextest run` over every agent package (`querymt-agent-core`, `-index`, `-knowledge`, `-sqlite`, `-services`, `-runtime` with `$RF`, `querymt-agent` with `$F`).
  - Run `cargo test --doc` for runtime and top, and fmt and clippy as in the group gate. Rerun any failing mesh timing test alone before treating it as a regression.
  - typeshare output equals `baseline/types.ts`.
  - Record the `line-counts.txt` delta against 1.3.
  - Time a one-line edit in runtime `delegation/core.rs` with `cargo test -p querymt-agent-runtime --features "$RF" --no-run`, the method of `split-agent-crate` 9.3.
- [ ] 9.5 Confirm the cleanup. Verify:
  - `grep -rn "AgentQuorum\|QuorumBuilder\|dyn AgentHandle\|AgentRegistry\|ProtocolTargetFactory\|DotagentsTargetFactory\|RemoteAgentHandle\|collect_event_sources\|ApprovalMode\|with_extension\|fn extensions" crates` returns nothing;
  - `git diff --stat main -- 'crates/agent*/src'` shows more lines removed than added.
- [ ] 9.6 `openspec validate unify-agent-runtime --strict` passes.

## Workflow follow-up

- A live check of `querymt/agents/add` and `querymt/agents/remove` needs a client that can send extension methods; `qmacp` cannot yet. Add an `ext` command there or check from `querymt-desktop`.
- `querymt-desktop`: controls to add and remove agents, show origin and location, react to `querymt/agents/changed`.
- Feed the 9.4 measurements into `split-agent-crate`'s open decision on splitting `querymt-agent-runtime` further.
- Archive `support-mcp-skills`, `refine-mcp-skill-approval` and this change after review.
