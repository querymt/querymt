# Changelog

## Unreleased

### Breaking: canonical snake_case parameters for the edit tools

The `edit`, `multiedit` and `replace_symbol` tools now use `snake_case` parameter names
like every other builtin tool. This is a single-release change: the schemas, the tool
implementations, file-access scheduling and dependent consumers all move together.

| Before | Now |
|---|---|
| `filePath` | `file_path` |
| `oldString` | `old_string` |
| `newString` | `new_string` |
| `replaceAll` | `replace_all` |
| `newText` (`replace_symbol`) | `new_text` |
| `expectedHash` (`replace_symbol`) | `expected_hash` |

- **Permanent aliases.** The camelCase names, plus `old_str` / `new_str`, are still
  accepted for these builtin tools (top level and inside `edits` / `replacements`
  items) and are renamed before the call is processed. They are not advertised to the
  model, and are not applied to MCP or provider tools.
- **Conflicting spellings are rejected.** Supplying two spellings of one parameter
  (for example `new_string` and `newString`) fails the call, even if the values match.
- **Closed schemas.** Builtin tool schemas set `additionalProperties: false`; unknown
  keys are rejected instead of ignored.
- **Pre-tool hooks receive canonical names.** Hooks that read `filePath`, `oldString`
  or `newString` must be updated to `file_path`, `old_string` and `new_string`.
- **Stored history is not rewritten.** Existing sessions keep their camelCase tool
  calls; readers of stored history must accept both spellings.
- Changing the tool schemas invalidates provider prompt caches once.

### Changed

- `search_text.exclude` accepts a single glob string as well as an array.
- `question.header` values longer than 12 characters are accepted and shortened to the
  first 12 Unicode scalar values before display, instead of failing validation.
- Arguments with unambiguous shape mistakes (a JSON string where an array or object is
  expected, an integer sent as a string, a single edit sent without its array) are
  repaired using the tool's schema before validation.

### Release coordination

The canonical naming release is one unit made of these dependent changes. Ship them
together; none of them is safe to release alone.

| Part | Where | What changes |
|---|---|---|
| Agent tools | `crates/agent` | Canonical snake_case schemas, permanent aliases, closed schemas, prepared-call scheduling, `INVALID_JSON` handling. |
| Hook guidance | `docs/docs/agent/hooks.md`, this changelog | `pre_tool_use` input uses canonical names; `updated_input` is normalised again. |
| Desktop readers | `querymt-desktop` (`src/lib/domain/session-tool-args.ts` and tests) | Readers accept canonical snake_case, historical camelCase, `old_str`/`new_str`, and an `edits` array stored as a JSON string. Executed-target rendering uses `tool_call_start` arguments, which are the effective ones. |
| Embedded UI pin | `crates/agent/embedded-ui-revision` | Must point at a `querymt-desktop` revision that contains the reader change, otherwise the embedded dashboard renders new snake_case calls with the old readers. |

The pin is a release step, not part of this change: the desktop commit does not exist
until the desktop repository is committed and published, so bump
`embedded-ui-revision` to that revision before tagging.

Compatibility is checked against historical records on both sides. The desktop suite
has fixtures for camelCase, snake_case, short-form, stringified-array and
hook-redirected calls, and the agent tests cover raw assistant history alongside
effective execution records.

### Rollback

Roll the canonical naming release back as a coordinated unit: the agent, the pre-tool
hook guidance and the desktop readers, including the embedded UI pin. Do not swap the
alias direction alone: schemas, tool readers, file-access scheduling and consumers must
all return to the same convention together. Whatever is rolled back, keep accepting
both historical spellings in stored history, because sessions recorded under either
release stay in the database and raw assistant history is never rewritten.

Anthropic strict tools roll back separately: `strict_tools = []` disables selection by
name, `oauth_strict_tools = false` closes the OAuth gate, and calls that set
`strict: true` explicitly need their callers changed or provider strict support
reverted.
