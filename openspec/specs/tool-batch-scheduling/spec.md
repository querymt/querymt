# tool-batch-scheduling Specification

## Purpose

Defines how the agent executes multiple tool calls emitted in a single model response: which calls may run in parallel, which must run sequentially, and in what order, so that concurrent calls never corrupt each other's file changes.

## Requirements

### Requirement: Same-file mutations never run concurrently

When a single model response contains two or more tool calls that write the same file, the system SHALL execute them one after another, never concurrently. Every successful write MUST be reflected in the file after the batch completes.

#### Scenario: Two edits to the same file in one response

- **WHEN** one model response contains two `edit` calls on `design.md` with non-overlapping `oldString` values
- **THEN** both calls succeed and the final `design.md` contains both replacements

#### Scenario: Different spellings of the same path

- **WHEN** one response contains an `edit` on `./docs/a.md` and a `write_file` on the absolute path of `docs/a.md`
- **THEN** the system treats them as the same file and runs them sequentially

#### Scenario: Multi-file replacement conflicts with a single-file edit

- **WHEN** one response contains a `replace_symbol` call touching `a.rs` and `b.rs` and an `edit` call on `b.rs`
- **THEN** the two calls run sequentially

### Requirement: Conflicting calls preserve model order

Calls that must run sequentially because of a file conflict SHALL execute in the order the model emitted them, and their results SHALL be returned in that same order.

#### Scenario: Later edit depends on earlier edit

- **WHEN** a response contains edit A followed by edit B on the same file, and B's `oldString` only exists after A is applied
- **THEN** A runs first, B runs second, and both succeed

#### Scenario: Result order matches call order

- **WHEN** a batch is partly serialized because of conflicts
- **THEN** the tool results are recorded in the same order as the original tool calls

### Requirement: Non-conflicting calls keep running in parallel

Tool calls that do not conflict on any file SHALL continue to execute concurrently. Read-only access to the same file by multiple calls MUST NOT be serialized.

#### Scenario: Edits to different files

- **WHEN** one response contains `edit` calls on three different files
- **THEN** the three calls run concurrently

#### Scenario: Parallel reads of one file

- **WHEN** one response contains several read-only calls on the same file and no writes to it
- **THEN** the calls run concurrently

### Requirement: Read and write of the same file are ordered

A call that reads a file and a call that writes that same file in one response SHALL run sequentially in model order, so the read observes a well-defined state rather than a partial write.

#### Scenario: Read after edit

- **WHEN** a response contains an `edit` on `x.rs` followed by a read of `x.rs`
- **THEN** the read runs after the edit completes and observes the edited content

#### Scenario: Symbol and outline readers

- **WHEN** a response contains an `edit` on `x.rs` followed by `get_function`, `get_symbol` or `index` on `x.rs`
- **THEN** the reader runs after the edit completes

#### Scenario: Directory search waits for writes beneath it

- **WHEN** a response contains an `edit` on `src/a.rs` followed by `search_text`, `ls` or `glob` over `src`
- **THEN** the search runs after the edit completes, while a search over an unrelated directory runs concurrently

#### Scenario: Filtered search ignores writes it cannot observe

- **WHEN** a response contains an `edit` on `src/a.rs` and a `search_text` over `src` with `include = "*.md"`
- **THEN** the two calls run concurrently

#### Scenario: Filtered search still waits for matching writes

- **WHEN** a response contains an `edit` on `src/a.md` followed by a `search_text` over `src` with `include = "*.md"`
- **THEN** the search runs after the edit completes

#### Scenario: Deleting a directory under a filtered search

- **WHEN** a response deletes the directory `src/docs` and then runs `search_text` over `src` with `include = "*.md"`
- **THEN** the search runs after the deletion completes

#### Scenario: Grouped batches cost one extra wave regardless of order

- **WHEN** a response contains a workspace search and two reads followed by three edits of other files, or the same calls in the opposite order
- **THEN** the calls run in two waves in both cases

### Requirement: Shell commands do not overlap with file-writing tools

A tool call whose file access cannot be determined (such as a shell command) SHALL NOT run concurrently with any call that writes a file in the same response. It MAY run concurrently with read-only or non-file calls, and with other undetermined-access calls.

#### Scenario: Shell and edit in one response

- **WHEN** a response contains an `edit` on `a.md` followed by a `shell` call
- **THEN** the shell call starts only after the edit finishes

#### Scenario: Shell alongside reads

- **WHEN** a response contains a `shell` call and two read-only calls
- **THEN** all three run concurrently

### Requirement: Tools without declared file access keep current behaviour

Tool calls from tools that do not declare file access SHALL be scheduled as before: they run in parallel unless their execution class requires otherwise.

#### Scenario: Third-party tool

- **WHEN** a response contains a custom tool that declares no file access alongside an `edit`
- **THEN** the custom tool runs concurrently with the edit

### Requirement: Existing execution barriers are preserved

Serial-stateful and clarification-boundary tool calls SHALL keep acting as ordering barriers, and cancellation SHALL still produce exactly one result per tool call, including for calls that had not started because they were waiting on a conflict.

#### Scenario: Cancellation while a conflicting call waits

- **WHEN** the user cancels while edit B waits for edit A on the same file
- **THEN** every call in the batch, including B, has exactly one result, and B's result reports cancellation

#### Scenario: Clarification boundary still stops the batch

- **WHEN** a clarification answer changes the run objective mid-batch
- **THEN** the remaining calls are skipped with a result, as before

### Requirement: File conflicts are detected from normalised arguments
File-access detection and conflict guards SHALL use the final effective arguments after normalisation and pre-tool hook updates. A call naming its target through an alias, repair or hook replacement SHALL be treated as accessing that target. Execution SHALL consume those prepared arguments without repeating the pre-tool hook.

#### Scenario: Alias and canonical name on the same file
- **WHEN** one response contains an `edit` on `a.rs` using `file_path` and an `edit` on `a.rs` using `filePath`
- **THEN** the two calls run sequentially in model order and both replacements are in `a.rs`

#### Scenario: Stringified edits still conflict
- **WHEN** one response contains a `multiedit` on `a.rs` whose `edits` is a JSON string and an `edit` on `a.rs`
- **THEN** the two calls run sequentially

#### Scenario: Conflicting alias call
- **WHEN** a call is rejected because it contains both an alias and its canonical name
- **THEN** it is treated as a call without file access and returns its error without invoking the tool or preventing unrelated calls from running

#### Scenario: Hook redirects disjoint edits to one target
- **WHEN** two edits initially target different files and pre-tool hooks redirect both to `shared.rs`
- **THEN** conflict detection uses `shared.rs`, the calls cannot execute concurrently, and each pre-tool hook runs once

#### Scenario: Conflict guard sees repaired targets
- **WHEN** a configured conflict guard inspects calls whose targets become equal only after normalisation or a hook update
- **THEN** it applies its configured conflict policy to the final effective targets rather than approving them as disjoint

### Requirement: Preparation preserves execution boundaries
Argument preparation SHALL respect stateful and clarification execution barriers and cancellation. Calls after a barrier SHALL NOT invoke pre-tool hooks until execution can proceed beyond it. Calls skipped due to cancellation or successful clarification SHALL receive the existing ordered synthetic results without invoking their hooks or tools. Every call SHALL retain exactly one result in model order.

#### Scenario: Stateful boundary changes preparation context
- **WHEN** a stateful tool changes session context before a following edit
- **THEN** the following edit's pre-tool hook and file-access preparation observe the updated context

#### Scenario: Clarification stops later calls
- **WHEN** a successful clarification boundary causes later calls in the batch to be skipped
- **THEN** those calls' pre-tool hooks and tools do not run and each receives one synthetic result

#### Scenario: Cancellation before preparation
- **WHEN** cancellation is observed before a pending call is prepared
- **THEN** that call's pre-tool hook and tool do not run and its ordered cancellation result is preserved
