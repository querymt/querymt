---
name: querymt-dev
description: Implement, debug, and validate QueryMT backend changes autonomously using focused tests, isolated qmtcode instances, ACP checks, and runtime evidence. Use for local agent/provider/ACP/mesh development and runtime regressions.
version: "0.1.0"
---

# QueryMT Development

Close the implementation-to-validation loop yourself where the available tools
permit it. Do not hand the user a manual backend test checklist when you can
execute the relevant check in the development environment. This is a selectable workflow, not a requirement
to start a lab, run every suite, or collect traces for every change.

## Working contract

- Follow repository instructions and preserve existing work. Detect Nix first;
  when available, use the project's Nix environment on Linux or macOS. Otherwise
  use the supported native toolchain. Use Rust guidance for Rust changes.
- State the behavior to prove, its smallest useful regression, and any live gate.
  Tests must reach the failing production boundary, not reproduce your hypothesis.
- Complete implementation and local validation autonomously. Ask when blocked by
  a missing requirement or unavailable dependency, not before routine checks.
- Never write to, migrate, replace or delete the user's existing session database.
  Use a fresh database or a consistent copy; all test writes stay in the test database.
- Never stop or restart user-run QueryMT services or pre-existing `qmtcode` processes.
  Keep candidate binaries, state and endpoints separate. Only stop processes created
  for this test; if database identity or process ownership is unclear, pause and ask.
- Test the required behavior; do not remove mesh coverage or substitute a provider
  just to get green.
- Bound runs and retries. After uncertain submission, inspect state before retrying.
  Distinguish application, client, fixture, environment and collector failures.
- Do not implement or test UI under this skill. If a backend change needs UI
  validation, notify the user of the affected behavior and required follow-up in
  the UI repository. Complete independent backend checks; do not claim UI coverage.
- Review the final diff and report the tested revision, results, gaps and cleanup.
  Follow the task's commit/PR workflow; CI green is not live proof.

## Read only what you need

| Task | Reference |
|---|---|
| Select environment/checks, implement, review and finish | [Development loop](development.md) |
| Start or rebuild an isolated candidate on Linux/macOS | [Runtime lab](runtime.md) |
| Check client availability and select protocol assertions | [ACP checks](acp.md) |
| Test distributed ownership, routing or reconnect | [Mesh checks](mesh.md) |
| Explore logs, database records or optional OpenTelemetry data | [Runtime evidence](telemetry.md) |

Start with the development loop; load another reference only at that boundary.
For ACP operations, load the existing qmacp skill and verify the binary rather
than duplicating its instructions here. If a test is blocked, finish
independent checks and report the exact missing capability and unproved behavior.
