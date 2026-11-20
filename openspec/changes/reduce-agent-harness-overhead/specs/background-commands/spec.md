# Spec Delta

## Purpose

Lets a session run shell commands that outlive a tool call, and tells the session when they finish, so the model never has to block or poll with `sleep` while long work runs.

## ADDED Requirements

### Requirement: Shell commands can start in the background
The `shell` tool SHALL accept `background` (boolean, default false). With `background = true` the call SHALL return as soon as the command has started, with a result containing `job_id`, `status = "running"` and `output_path`. The command SHALL keep running after the call returns and after the run that started it ends. A command that cannot be started SHALL produce an error result and no job.

#### Scenario: Background start returns immediately
- **WHEN** the model calls `shell` with `command = "sleep 600; echo done"` and `background = true`
- **THEN** the call returns within a second with a `job_id`, `status = "running"` and an `output_path`

#### Scenario: Background command outlives its run
- **WHEN** a run starts a background command and then ends with a final answer
- **THEN** the command is still running and the session's runtime state lists it

#### Scenario: Background start failure
- **WHEN** the model calls `shell` with `background = true` and a `workdir` that does not exist
- **THEN** the call returns an error result and no background command is recorded

### Requirement: Long foreground commands move to the background
A `shell` call without `background = true` that is still running after `foreground_limit_secs` SHALL return a result with `job_id`, `status = "running"`, `moved_to_background = true`, `output_path` and the output produced so far, and the command SHALL continue as a background command without being restarted. When `foreground_limit_secs` is 0, a foreground call SHALL wait until the command exits.

#### Scenario: Foreground command exceeds the limit
- **WHEN** `foreground_limit_secs = 240` and the model calls `shell` with `command = "sleep 3000; date"`
- **THEN** the call returns after about 240 seconds with `moved_to_background = true` and a `job_id`
- **AND** the command keeps running and is not started a second time

#### Scenario: Foreground command finishes within the limit
- **WHEN** `foreground_limit_secs = 240` and a command exits after 30 seconds
- **THEN** the call returns the exit code and output and no background command is recorded

#### Scenario: Limit disabled
- **WHEN** `foreground_limit_secs = 0` and a command runs for 20 minutes
- **THEN** the call returns only when the command exits

### Requirement: Background command output is kept in a file
The standard output and standard error of a background command SHALL be appended, in arrival order, to the file at `output_path` while the command runs. The file SHALL be readable with the session's file tools while the command runs and after it exits. For a command moved from the foreground, the file SHALL also contain the output produced before the move.

#### Scenario: Output is readable while the command runs
- **WHEN** a background command has printed three lines and is still running
- **THEN** reading `output_path` returns those three lines

#### Scenario: Moved command keeps its earlier output
- **WHEN** a foreground command prints `starting`, is moved to the background and later prints `finished`
- **THEN** `output_path` contains `starting` followed by `finished`

### Requirement: An active run is told when a background command exits
When a background command exits while its session has an active run, the agent SHALL add an exit notice to the conversation before the run's next model request. The notice SHALL contain the `job_id`, the command, the exit code, the run time in seconds, `output_path` and at most the last 4 KiB of output. Exits that occur before the same model request SHALL be delivered together. A notice SHALL never be placed between a tool call and its result.

#### Scenario: Exit during a tool batch
- **WHEN** a background command exits with code 0 while the run executes other tool calls
- **THEN** the next model request contains the results of those calls followed by one exit notice naming the `job_id` and exit code 0

#### Scenario: Two exits before one request
- **WHEN** two background commands exit before the run's next model request
- **THEN** that request contains both exit notices, in exit order

#### Scenario: Exit notice for a failing command
- **WHEN** a background command exits with code 2 after printing 100 KiB of output
- **THEN** the notice reports exit code 2, at most the last 4 KiB of output, and the `output_path` holding all of it

### Requirement: An idle session is woken when a background command exits
The `shell` tool SHALL accept `wake_on_exit` (boolean, default true), which applies to background commands and to commands moved to the background. When such a command exits while its session has no active run, the agent SHALL start a run whose input is the exit notice. With `wake_on_exit = false` no run SHALL be started, and the notice SHALL be delivered before the first model request of the session's next run.

#### Scenario: Idle session is woken
- **WHEN** a run starts a background command and ends, and the command exits 40 minutes later
- **THEN** a new run starts in that session with the exit notice as its input

#### Scenario: Wake-up run is not a user prompt
- **WHEN** a run is started by a background command exit
- **THEN** its run-start record identifies a background command exit, not a user prompt, as its origin

#### Scenario: Wake-up disabled
- **WHEN** a command started with `wake_on_exit = false` exits while the session is idle
- **THEN** no run starts, and the next run the user starts receives the exit notice before its first model request

#### Scenario: Several exits wake one run
- **WHEN** two background commands exit within the same second while the session is idle
- **THEN** one run starts and its input contains both exit notices

### Requirement: The model can wait for background commands
The `job_wait` tool SHALL accept `job_ids` (array of job ids; when omitted, every running background command of the session) and `timeout_secs` (integer, 1 to 240, default 60). It SHALL return when any named command has exited or the timeout elapses, whichever is first, reporting for each named command its status, exit code if exited, and at most the last 4 KiB of output. An exit reported by `job_wait` SHALL NOT also be delivered as an exit notice.

#### Scenario: Wait returns on exit
- **WHEN** the model calls `job_wait` for a command that exits 5 seconds later with code 0
- **THEN** the call returns after about 5 seconds with that command's status `exited` and exit code 0
- **AND** no separate exit notice is delivered for it

#### Scenario: Wait times out
- **WHEN** the model calls `job_wait` with `timeout_secs = 30` for a command that keeps running
- **THEN** the call returns after about 30 seconds with status `running` and the latest output

#### Scenario: Wait with nothing running
- **WHEN** the model calls `job_wait` without `job_ids` and the session has no running background command
- **THEN** the call returns immediately and reports that nothing is running

#### Scenario: Wait timeout out of range
- **WHEN** the model calls `job_wait` with `timeout_secs = 3000`
- **THEN** the call fails with an error naming `timeout_secs` and its allowed range, and nothing is waited for

#### Scenario: Unknown job id in wait
- **WHEN** the model calls `job_wait` with a `job_id` the session never started
- **THEN** the call fails with an error naming that id

### Requirement: The model can stop a background command
The `job_kill` tool SHALL accept `job_id` and SHALL stop that command together with every process it started, then return its final status and at most the last 4 KiB of output. Stopping a command that has already exited SHALL return its recorded status without an error. A command stopped by `job_kill` SHALL NOT wake the session and SHALL NOT produce an exit notice.

#### Scenario: Kill a running command
- **WHEN** the model calls `job_kill` for a running command that started child processes
- **THEN** the command and its children are no longer running and the result reports status `killed`

#### Scenario: Kill an exited command
- **WHEN** the model calls `job_kill` for a command that exited earlier with code 0
- **THEN** the result reports status `exited` and exit code 0

#### Scenario: Killed command does not wake the session
- **WHEN** a command is stopped with `job_kill` and the run then ends
- **THEN** no run is started for that command and no exit notice is delivered for it

### Requirement: Background commands belong to one session
A background command SHALL be visible to and controllable by only the session that started it. `job_wait` and `job_kill` SHALL be offered to the model exactly when `shell` is offered. Cancelling a run SHALL NOT stop the session's background commands. Closing or deleting the session, a delegation of that session reaching a terminal state, or shutdown of the agent process SHALL stop them.

#### Scenario: Another session cannot control the command
- **WHEN** session B calls `job_kill` with a `job_id` started by session A
- **THEN** the call fails as an unknown job id and session A's command keeps running

#### Scenario: Job tools follow the shell tool
- **WHEN** a session's allowed tools do not include `shell`
- **THEN** neither `job_wait` nor `job_kill` is offered to the model

#### Scenario: Cancel leaves background commands running
- **WHEN** the user cancels a run while a background command of that session is running
- **THEN** the run is cancelled and the command keeps running

#### Scenario: Closing the session stops its commands
- **WHEN** a session with a running background command is closed
- **THEN** the command and its child processes are stopped

#### Scenario: Finished delegation stops the delegate's commands
- **WHEN** a delegate session's delegation completes while that session has a running background command
- **THEN** the command is stopped and the delegate session is not woken

### Requirement: Background commands are recorded and observable
The agent SHALL record a durable `background_command_started` event (job id, command, originating tool call id, output path, whether it was moved from the foreground) when a command becomes a background command, and one durable `background_command_exited` event (job id, exit code if any, run time, reason `exited`, `killed` or `lost`) when it ends. The session's runtime state SHALL list every running background command with its job id, command and start time.

#### Scenario: Start and exit are journaled
- **WHEN** a background command starts and later exits with code 0
- **THEN** the journal contains one `background_command_started` and one `background_command_exited` event for its job id, the latter with reason `exited`

#### Scenario: Runtime state lists running commands
- **WHEN** a client reads the runtime state of a session with one running background command
- **THEN** the state lists that command with its job id, command and start time

#### Scenario: Command lost with its process
- **WHEN** the agent process dies while a background command is running and the session is later loaded
- **THEN** the journal gains one `background_command_exited` event for that job id with reason `lost`
- **AND** the session is not woken for it

### Requirement: Background command settings are validated
`[agent.execution.shell]` SHALL accept `foreground_limit_secs` (integer seconds, 0 to 86400, default 240; 0 disables moving) and `max_background` (integer, 1 to 64, default 8; the number of background commands one session may have running). A value outside its range or of the wrong type, or an unknown key in the table, SHALL be a load error naming the key. Starting a command beyond `max_background` SHALL return an error result naming the limit, and the command SHALL NOT start.

#### Scenario: Shell settings default when omitted
- **WHEN** a configuration has no `[agent.execution.shell]` table
- **THEN** it loads with `foreground_limit_secs = 240` and `max_background = 8`

#### Scenario: Invalid shell setting is rejected
- **WHEN** `[agent.execution.shell]` sets `max_background = 0` or `foreground_limit_secs = -5`
- **THEN** loading fails with an error naming the offending key

#### Scenario: Too many background commands
- **WHEN** `max_background = 2`, two background commands are running and the model starts a third with `background = true`
- **THEN** the call returns an error naming the limit of 2 and no third command starts
