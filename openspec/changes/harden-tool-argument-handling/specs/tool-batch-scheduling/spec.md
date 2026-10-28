# Spec Delta

## ADDED Requirements

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
