# Development loop

## Choose the proof before the patch

Inspect the actual failure and current code, not only a previous agent's summary.
Use relevant prior sessions/history when needed. Write a short acceptance statement
with the expected state change, failure case, and scope. Decide whether the check needs a real process,
provider or second node. Most changes need only one of those.

| Boundary changed | Smallest useful proof | Expand when needed |
|---|---|---|
| Pure Rust logic | Focused test through the production function | Affected consumers/features |
| Provider request/stream | Final wire shape and stream/result assertions | Rebuilt plugin with bounded affected-provider replay |
| DB/history | Persistence invariants on a fixture or consistent copy | ACP new/list/load against an isolated server |
| ACP/runtime | Deterministic provider + real client state/events | Queue, cancel, interaction or reconnect gate |
| Mesh ownership/routing | Real separated processes with required mesh enabled | Two hosts when process-local tests cannot model the failure |
| Telemetry | SDK capture for types/parentage/privacy | Bounded export check if suitable tooling is available; otherwise report export unverified |

Prefer an existing deterministic provider fixture over paid inference for lifecycle
checks. It must implement the wire behavior under test, not bypass provider code.
Use the configured real provider when the bug depends on its behavior. Assert
outcomes and invariants, not exact LLM prose.

## Use the selected environment

Follow the environment-selection rule in [the entry skill](SKILL.md). If Nix applicable,
enter the project devShell with `nix develop`, or run commands with
`nix develop -c <command>`; an already active project direnv/devShell is sufficient.
Diagnose setup failures rather than bypassing the selected environment.
Run the commands below from the repository root inside that environment.

## Implement and check

1. Capture branch/status and scope; inspect related code before editing. Keep a
   regression that fails for the reported behavior, if practical. A broken fixture
   is not a production RED. Never remove the requirement to make the test pass.
2. Change the smallest coherent area. Delegate independent work if tools permit;
   keep one owner of the acceptance contract and avoid competing edits/builds.
3. From this repository root, use scoped commands, adapting package/features:
   ```sh
   cargo check -p querymt-agent
   cargo test -p querymt-agent --lib TEST_FILTER
   cargo fmt --all -- --check
   ```
   Replace `TEST_FILTER` with the actual test/module and verify tests ran. Required
   mesh tests need `--features remote`; required reduced-feature tests need their
   real flags. Run `cargo check` after Rust edits and before formatting. No Rust
   build is needed merely to edit this skill or other prose.
4. Expand to relevant neighbors and the applicable CI feature set before handoff.
   Agent CI-shaped lint is:
   ```sh
   cargo clippy -p querymt-agent --all-targets --features dashboard,oauth,remote -- -D warnings
   ```
   Read the current workflow. Its small `QMT_UI_DIST` fixture satisfies backend
   build requirements without building/testing the separate frontend repository.
   Use bounded nextest/runtime waits where appropriate; do not repeatedly build
   every example/provider or create a new target directory for each minor edit.
5. Rebuild the candidate after the final runtime-affecting edit and run the chosen
   acceptance gate. Review the final diff, not only test counts or delegate claims.

If a suite fails due to HOME-global skills, shared runtime state or build resource
pressure, isolate the cause. An unrelated failure is a limitation to report, not
permission to hide failures or add blanket lint allowances. After a timeout,
inspect progress/state once and adjust the diagnosed cause, not just the timeout.

## Finish with evidence

If the change affects a UI-facing contract, report that contract, backend evidence
and the specific UI follow-up needed to the user. UI implementation and testing
belong to the separate repository/workflow; do not perform them under this skill.

Keep the smallest useful record: revision/dirty patch, features and actual binary,
commands with exit codes and assertions, relevant session/run/trace IDs, exclusions
and cleanup. Preserve a failed harness assertion separately from its correction.
Use durable history for long-running work; simple edits need no elaborate evidence
bundle.

When publication is requested, follow the requested commit/PR style and draft/base
status, monitor checks on the exact published head and fix confirmed issues. A
coverage-report exception does not excuse failed tests run under coverage. A bot
skipping a draft is not a review. Report remaining live gaps honestly.
