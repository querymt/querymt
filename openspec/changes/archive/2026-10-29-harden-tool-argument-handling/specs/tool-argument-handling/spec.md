# Spec Delta

## Purpose

Defines how tool arguments are normalised, validated and reported before execution, with builtin-only aliases and wrapping and generic schema-driven value repairs. Raw model output remains available for replay while effective arguments govern execution.

## ADDED Requirements

### Requirement: Builtin tool parameters use snake_case names
Every property name in a builtin tool's parameter schema, including properties of nested objects and array items, SHALL match `^[a-z][a-z0-9_]*$`. The edit-tool naming change SHALL expose snake_case schemas and accept legacy aliases in the same release, without an intermediate camelCase canonical convention.

#### Scenario: Edit tool schema
- **WHEN** the definition of the `edit` tool is sent to a provider
- **THEN** its parameters are `file_path`, `old_string`, `new_string` and `replace_all`

#### Scenario: Nested item names
- **WHEN** the definitions of `multiedit` and `replace_symbol` are sent to a provider
- **THEN** each `edits` item has `old_string`, `new_string` and `replace_all`, and each `replacements` item uses `new_text` and `expected_hash`

#### Scenario: Naming guard
- **WHEN** a builtin tool is added with a camelCase property anywhere in its schema
- **THEN** the test suite fails and names the tool and property

### Requirement: Declared aliases are accepted for builtin tool parameters
A builtin tool MAY declare aliases for its parameters. When a call contains an alias and not the canonical name at the same level, the system SHALL rename the alias to the canonical name before the call is processed. Aliases SHALL apply at the top level and inside nested items. Aliases SHALL NOT be offered for tools outside the builtin set.

#### Scenario: camelCase name on a renamed tool
- **WHEN** an `edit` call contains `filePath`, `oldString` and `newString`
- **THEN** the call runs as if it contained `file_path`, `old_string` and `new_string`

#### Scenario: Short editor-style names
- **WHEN** an `edit` call contains `file_path`, `old_str` and `new_str`
- **THEN** the call runs as if it contained `old_string` and `new_string`

#### Scenario: Alias inside nested items
- **WHEN** a `replace_symbol` call has a `replacements` item with `newText`
- **THEN** that item is processed as if it contained `new_text`

#### Scenario: MCP tool keys are untouched
- **WHEN** an MCP tool call contains a key named `filePath`
- **THEN** the key is passed to the MCP server unchanged

### Requirement: Conflicting alias and canonical names are rejected
When a call contains multiple spellings of the same parameter at the same level, including two aliases or an alias and its canonical name, the system SHALL NOT run the tool and SHALL return a tool error naming the conflicting keys, even if their values are equal.

#### Scenario: Both names present
- **WHEN** an `edit` call contains both `new_string` and `newString`
- **THEN** the tool does not run and the error result names `new_string` and `newString`

### Requirement: Effective arguments govern execution
Arguments SHALL be normalised before pre-tool hooks and again after a hook replaces the input. Final effective arguments SHALL govern validation, file-access scheduling, permissions, tool invocation, snapshots, execution events, progress records, post-tool hooks and tool-result argument records. Pre-tool hooks SHALL run at most once per call.

#### Scenario: Hook input
- **WHEN** an `edit` call uses `filePath` and a pre-tool hook is configured
- **THEN** the hook receives `file_path`

#### Scenario: Hook replacement is normalised
- **WHEN** a pre-tool hook replaces an edit's arguments using `filePath` and `newString`
- **THEN** validation, scheduling and execution use the replacement's canonical names and values, without invoking the pre-tool hook again

#### Scenario: Effective arguments are recorded
- **WHEN** an `edit` call using `newString` runs after a hook changes its target
- **THEN** its execution event and stored tool-result arguments contain canonical names and the final target

### Requirement: Raw assistant arguments remain immutable
Canonical assistant output and stored assistant history SHALL retain the model-emitted argument text for diagnostics and provider replay. Effective execution arguments SHALL be recorded separately and SHALL NOT overwrite the raw assistant call.

#### Scenario: Repair does not rewrite history
- **WHEN** a model emits a call with a stringified `edits` array that is repaired and executed
- **THEN** stored assistant output retains the emitted argument text while execution records contain the repaired array

### Requirement: Unambiguous value-shape mistakes are repaired using the schema
Before validation, the system SHALL repair unambiguous schema-declared shapes: decode a JSON string into its expected array or object, parse an integer string after trimming whitespace and trailing commas, or wrap a string as one item when an array of strings is expected. Builtins with a declared wrapper SHALL wrap matching flat item fields. Union schemas SHALL NOT trigger value repair. String-typed content SHALL remain unchanged.

#### Scenario: Edits sent as a JSON string
- **WHEN** a `multiedit` call has `edits` set to a string containing a valid JSON array of edit objects
- **THEN** the string is replaced by the parsed array and the call runs

#### Scenario: Number sent as a string
- **WHEN** a `read_tool` call has `offset: "466, "`
- **THEN** the call runs with `offset` 466

#### Scenario: Single edit without wrapper
- **WHEN** a `multiedit` call has `file_path`, `old_string` and `new_string` at the top level and no `edits`
- **THEN** the call runs with one edit made from those fields

#### Scenario: Single replacement without wrapper
- **WHEN** a `replace_symbol` call has `path`, `symbol` and `new_text` at the top level and no `replacements`
- **THEN** the call runs with one replacement made from those fields

#### Scenario: Broken JSON is not repaired
- **WHEN** a `multiedit` call has `edits` set to a string that is not valid JSON
- **THEN** the value is left unchanged and the call fails validation

#### Scenario: Edit contents are never touched
- **WHEN** an `edit` call has `new_string` set to text that is itself valid JSON
- **THEN** `new_string` is passed to the tool as the original string

### Requirement: Normalisation composes and is idempotent
Normalisation SHALL handle aliases inside decoded containers and flat fields before single-item wrapping, then process the resulting items against their schemas. Repeating normalisation on its output SHALL produce the same value with no additional repairs. Conflicting spellings inside decoded or wrapped objects SHALL be rejected.

#### Scenario: Alias inside a stringified array
- **WHEN** `multiedit.edits` contains the JSON string `[{"oldString":"a","newString":"b"}]`
- **THEN** effective `edits` is an array containing `old_string` and `new_string`, and the otherwise valid call passes validation

#### Scenario: Alias on an unwrapped replacement
- **WHEN** a `replace_symbol` call supplies `path`, `symbol` and `newText` without `replacements`
- **THEN** effective arguments contain one replacement using `new_text`

#### Scenario: Repeated normalisation
- **WHEN** already normalised arguments are normalised again without a hook changing them
- **THEN** their value is unchanged and no additional repair is recorded

#### Scenario: Conflict inside decoded input
- **WHEN** a stringified edit item contains both `newString` and `new_string`
- **THEN** the tool does not run and the error names both keys

### Requirement: Invalid outer arguments never execute
For every provider, malformed JSON, non-object outer arguments and calls marked invalid by the provider SHALL be rejected before pre-tool hooks, permission checks or invocation. Each rejected call SHALL receive one error result containing `{"INVALID_JSON": "<raw input>"}`, with the raw diagnostic shortened to at most 2,000 Unicode scalar values. Invalid input SHALL NOT silently become an executable empty object.

#### Scenario: Malformed outer JSON
- **WHEN** a call's raw arguments are `{"edits": [`
- **THEN** hooks and the tool are not invoked and one error result contains `INVALID_JSON`

#### Scenario: Non-object arguments
- **WHEN** a call's raw arguments parse as an array
- **THEN** hooks and the tool are not invoked and one error result contains `INVALID_JSON`

#### Scenario: Provider marker overrides valid syntax
- **WHEN** a provider marks a call invalid even though its raw arguments parse as an object
- **THEN** the invalid status survives execution preparation and the call receives an error without running hooks or the tool

### Requirement: Search exclusions accept a single pattern
The `search_text` tool SHALL accept `exclude` as either one glob string or an array of glob strings. A single string SHALL behave like an array with that one element.

#### Scenario: Single exclude pattern
- **WHEN** a `search_text` call has `exclude: "**/tests/**"`
- **THEN** the search runs and skips files matching `**/tests/**`

### Requirement: Over-long question headers are shortened
The `question` tool SHALL accept a string header through argument validation regardless of its length and shorten it to the first 12 Unicode scalar values before presentation. This SHALL NOT introduce generic truncation of other string arguments.

#### Scenario: Long header
- **WHEN** a `question` call has header `Deployability`
- **THEN** the question is shown with a 12-character header and the call does not fail

#### Scenario: Multibyte header through agent execution
- **WHEN** an otherwise valid `question` call has a header containing 13 multibyte Unicode scalar values and is submitted through the agent
- **THEN** argument validation succeeds and the displayed header contains the first 12 scalar values without corrupting UTF-8

### Requirement: Unknown parameters are rejected
Builtin tool schemas SHALL forbid properties that are neither declared nor declared aliases, at every object level. A call containing such a property SHALL fail validation instead of having the property ignored.

#### Scenario: Unknown key on edit
- **WHEN** an `edit` call contains a key `replace_everything`
- **THEN** the tool does not run and the error names `replace_everything` as not allowed

### Requirement: Validation errors are complete, short and actionable
When a call fails validation, the error result SHALL list every violation with its argument path and the expected type or constraint. Any echoed argument value SHALL be at most 80 characters, marked as shortened when cut. Known mistakes SHALL carry a hint.

#### Scenario: Several violations
- **WHEN** a call is missing two required fields
- **THEN** the error lists both fields

#### Scenario: Large invalid value
- **WHEN** `edits` is an invalid string of 2,000 characters
- **THEN** the error echoes at most 80 characters of it

#### Scenario: Stringified array hint
- **WHEN** `edits` is a string that could not be repaired
- **THEN** the error states that `edits` must be a JSON array of objects, not a string containing JSON

#### Scenario: Misspelled name hint
- **WHEN** an unknown key differs from a declared name only by case style or a known short form
- **THEN** the error suggests the declared name

### Requirement: Argument repairs are observable
Each alias rename and value repair SHALL be recorded in telemetry with the tool name and rule and attached to the tool-execution trace span. Telemetry SHALL distinguish repaired calls that succeed from repaired calls that fail. Scheduling inspections and unchanged re-normalisation SHALL NOT increment repair counts.

#### Scenario: Alias counted
- **WHEN** an `edit` call is normalised from `newString`
- **THEN** a repair counter is incremented with tool `edit` and rule `alias`

#### Scenario: Clean call not counted
- **WHEN** a call needs no alias or repair
- **THEN** no repair is recorded

#### Scenario: Successful repairs are distinguishable
- **WHEN** one repaired call succeeds and another repaired call fails validation
- **THEN** telemetry identifies their distinct outcomes independently of per-rule repair counts
