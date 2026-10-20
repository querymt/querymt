# ACP checks

## Use the existing client skill

Before the first ACP check, look for and load the available qmacp skill; it may
still be advertised as `qmtacp`. Verify `command -v qmacp` and `qmacp --version`
in the selected environment. The client skill owns command syntax, capability
discovery, output handling, permissions and interaction policies.

Only if the skill or binary is unavailable, use the
[qmacp installation instructions](https://github.com/querymt/qmacp#install) to install
the missing component; do not reinstall components already available. Do not invent a replacement command catalog here. Finish independent
backend tests while a client check is blocked; use installed help to resolve
version-specific differences.

## Select the observations

Use the isolated candidate endpoint and fixture workspace. Discover capabilities
and IDs rather than guessing them. Select only the assertions the change needs:

- Session/config: server-accepted settings and persisted/loadable state, not a
  client echo; actual provider routing when that is the behavior under test.
- Prompt/control: expected terminal outcome and semantic result; correlate
  session/run/input IDs with history and runtime. Acceptance is not completion.
- Cancel/reconnect: bounded termination or intended owner continuity, recovered
  state and no duplicate original submission. Inspect state before retrying.
- Interactions: verify the current client's policy can exercise the required
  permission/question behavior; automatic handling must not mask the test.

A capability missing from the client is not proof the server is broken. Report
that exact gap and the unproved scenario; do not weaken acceptance or silently
expand into another repository. Use the client skill for the execution details.
