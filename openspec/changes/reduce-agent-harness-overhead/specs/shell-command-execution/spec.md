# Spec Delta

## Purpose

Defines how a `shell` tool call completes, what its result contains when output is large, how cancellation is reported, and how the agent steers the model from shell one-liners toward dedicated tools.

## ADDED Requirements

### Requirement: A shell call returns when its command exits
A foreground `shell` call SHALL return within 2 seconds of its command's process exiting, with the exit code and the output captured up to that point, even when other processes still hold the command's output streams open. Processes the command left running SHALL NOT be stopped by the return, and output they write afterwards SHALL NOT be part of the result.

#### Scenario: Command leaves a detached child holding its output
- **WHEN** the model calls `shell` with `command = "sleep 300 & echo started"`
- **THEN** the call returns within 2 seconds with exit code 0 and output `started`
- **AND** the `sleep` process is still running

#### Scenario: Command without lingering processes
- **WHEN** the model calls `shell` with `command = "echo hello"`
- **THEN** the call returns exit code 0 and output `hello` with no added delay

#### Scenario: Output written just before exit is kept
- **WHEN** a command prints 1 MiB of output and exits immediately
- **THEN** the result is built from all of that output

### Requirement: Truncated shell output keeps the beginning and the end
When the output of a `shell` call exceeds the configured tool output limits, the result SHALL contain the exit code, the first part of the output, the last part of the output, a marker between them stating how many lines and bytes were omitted, and the path of the file holding the complete output. The last part SHALL be at least half of the kept output. Output within the limits SHALL be returned unchanged with no marker.

#### Scenario: Long build output
- **WHEN** a command prints 10,000 lines, ends with an error summary and exits with code 101, and the limit is 2,000 lines
- **THEN** the result contains exit code 101, the first lines, the final lines including the error summary, and a marker naming the omitted line count and the overflow file

#### Scenario: Error stream survives truncation
- **WHEN** a command prints 5 MiB to standard output and then one line to standard error
- **THEN** the result contains that standard error line

#### Scenario: Output within the limits
- **WHEN** a command prints 50 lines and the limit is 2,000 lines
- **THEN** the result contains all 50 lines and no truncation marker

### Requirement: Every shell call has exactly one end event
For every tool-call start event of a `shell` call, the session's event stream SHALL contain exactly one matching tool-call end event. A call ended by run cancellation SHALL have an end event marked as an error whose result reports the cancellation, emitted before the run's completion event. A call still running when its process died SHALL be closed when the interrupted run is reconciled, with a result reporting the interruption.

#### Scenario: Cancelled while running
- **WHEN** the user cancels a run while a `shell` call is executing `sleep 600`
- **THEN** the event stream contains one end event for that call, marked as an error and reporting cancellation, before the run-completion event

#### Scenario: Cancelled before the command started
- **WHEN** the user cancels a run while a `shell` call is waiting for an earlier call in the same batch
- **THEN** the event stream contains one start-less result for that call and no unmatched start event

#### Scenario: Process died during a call
- **WHEN** the agent process dies while a `shell` call is running and the session is later loaded
- **THEN** the journal gains one end event for that call, marked as an error and reporting the interruption

### Requirement: Shell commands know their session
Every command started by the `shell` tool, in the foreground or in the background, SHALL run with the environment variable `QMT_SESSION_ID` set to the public id of the session that started it. The variable SHALL override a value inherited from the agent process's own environment.

#### Scenario: Command reads its session id
- **WHEN** session `S` calls `shell` with `command = "echo $QMT_SESSION_ID"`
- **THEN** the output is the public id of `S`

#### Scenario: Inherited value is replaced
- **WHEN** the agent process was started with `QMT_SESSION_ID=other` and session `S` runs `echo $QMT_SESSION_ID`
- **THEN** the output is the public id of `S`

### Requirement: Shell results hint at dedicated tools
When `dedicated_tool_hints` is on and a `shell` command consists of exactly one invocation of a file-reading, searching or listing program (`cat`, `head`, `tail`, `sed -n`, `nl`, `grep`, `rg`, `find`, `ls`) with no pipe, redirection or command separator, and the session is offered the equivalent dedicated tool, the result SHALL end with one line naming that tool. The command SHALL still run and its output SHALL be unchanged. No other command SHALL receive a hint.

#### Scenario: Plain grep gets a hint
- **WHEN** the model calls `shell` with `command = "grep -rn TODO src"` and `search_text` is offered
- **THEN** the result contains the grep output followed by one line naming `search_text`

#### Scenario: Pipeline gets no hint
- **WHEN** the model calls `shell` with `command = "cargo check 2>&1 | grep error"`
- **THEN** the result contains no hint

#### Scenario: Dedicated tool is not offered
- **WHEN** the model calls `shell` with `command = "cat notes.md"` and `read_tool` is not offered to the session
- **THEN** the result contains no hint

#### Scenario: Remote command gets no hint
- **WHEN** the model calls `shell` with `command = "ssh nas 'cat /etc/hostname'"`
- **THEN** the result contains no hint

### Requirement: Dedicated tool hints can be turned off
`[agent.execution.shell]` SHALL accept `dedicated_tool_hints` (boolean, default true). When false, no `shell` result SHALL contain a dedicated-tool hint. A non-boolean value SHALL be a load error naming `dedicated_tool_hints`.

#### Scenario: Hints default to on
- **WHEN** a configuration does not set `dedicated_tool_hints`
- **THEN** it loads with `dedicated_tool_hints = true`

#### Scenario: Hints turned off
- **WHEN** `dedicated_tool_hints = false` and the model calls `shell` with `command = "ls src"`
- **THEN** the result contains the listing and no hint

#### Scenario: Invalid hint setting is rejected
- **WHEN** `[agent.execution.shell]` sets `dedicated_tool_hints = "sometimes"`
- **THEN** loading fails with an error naming `dedicated_tool_hints`
