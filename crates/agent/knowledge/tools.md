---
id: pilot-tools
title: Tool system
description: How tools are declared, how model arguments are normalized and validated, and how file access drives scheduling.
tags: tools, arguments, validation, scheduling, permissions
knowledge_paths: ["crates/agent/src/tools/**"]
knowledge_sources: ["crates/agent/src/tools/args.rs"]
---

# Tool system

## The Tool trait

A built-in tool implements `Tool` (`src/tools/context.rs`): a name, a schema
(`definition`, or `definition_for_cwd` when the schema depends on the workspace), the
capabilities it needs, an execution class, optional argument aliases and a declared
`file_access`. `ToolRegistry` (`src/tools/registry.rs`) finds tools by name. Built-in
tools live in `src/tools/builtins/`, one file per tool; `AgentToolContext`
(`src/tools/context_impl.rs`) is the agent's implementation of `ToolContext`, the
session-scoped handle every tool receives.

Execution classes: `ParallelSafe` (default), `SerialStateful`, `ClarificationBoundary`.
Anything not `ParallelSafe` runs alone.

## Effective arguments

`normalize_arguments` (`src/tools/args.rs`) turns what the model emitted into *effective*
arguments before validation, scheduling and execution. The raw model output is kept for
history and provider replay. Two kinds of repair exist: built-in-only, tool-declared
(alias renames, wrapping flat fields into a one-element array) and generic, schema-driven
(a JSON string where an array or object is declared, an integer string where an integer is
declared). Union schemas and string-typed values are never repaired, broken JSON is never
"fixed", and normalizing twice changes nothing.

## Validation

`src/tools/validation.rs` validates effective arguments against the tool's JSON schema
and reports every violation, one line each, with a bounded echo of the offending value and
a "did you mean" hint for near-miss key names. Malformed outer JSON is rejected earlier
as invalid input.

## File access and scheduling

Each call declares a `FileAccess`: none, paths it reads (as a `ReadScope`) and writes,
unknown, or any. `src/tools/path.rs` gives every spelling of a path one identity
(relative, absolute, through a symlink), so the scheduler and the parallel-edit guard
agree on when two calls touch the same file. `src/tools/read_scope.rs` records the filter
a directory walker (`search_text`, `glob`, `ls`) applies, so such a read waits only for
writes it could actually observe. Scheduling itself is in
[execution.md](execution.md).

## Tools worth knowing

`index` returns a structural outline of one file and refuses directories;
`get_symbol` and `get_function` return a symbol body; `repo_knowledge_*` read repository
documentation (see [repository-knowledge.md](repository-knowledge.md)); the legacy
`knowledge_*` tools work on session memory and are unrelated.
