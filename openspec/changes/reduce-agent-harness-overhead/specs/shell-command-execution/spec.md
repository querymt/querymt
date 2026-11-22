# Spec Delta

## Purpose

Defines when a `shell` tool call completes and how its cancellation is reported. What a `shell` result contains, and how large output is cut, are not changed here.

## ADDED Requirements

### Requirement: A shell call returns when its command exits
A `shell` call SHALL return within 2 seconds of its command's process exiting, with the exit code and the output captured up to that point, even when other processes still hold the command's output streams open. Processes the command left running SHALL NOT be stopped by the return, and output they write afterwards SHALL NOT be part of the result. The result SHALL keep its current form (`exit_code`, `stdout`, `stderr`).

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

#### Scenario: Call abandoned while a left-over process still writes
- **WHEN** a `shell` call is abandoned after its command exited, while a process the command left running keeps writing to the command's output
- **THEN** the output written after that point is discarded and not held in memory

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
