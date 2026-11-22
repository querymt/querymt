# Spec Delta

## Purpose

Defines how the agent's per-workspace code index keeps derived per-file results (symbols and function entries) across tool calls and process restarts. It also defines how those results stay equal to freshly computed ones, and how the index and the symbol tools behave when the cache is unavailable.

## ADDED Requirements

### Requirement: Index results persist across restarts

When a workspace index starts for a root whose cache holds results from an earlier process, the index SHALL reuse the stored per-file results for every file whose text is unchanged, without parsing that file again.

#### Scenario: Restart with unchanged files parses nothing

- **GIVEN** a workspace index was built for a root with several supported source files and then shut down
- **WHEN** a new workspace index starts for the same root with the same cache directory and no file has changed
- **THEN** the index statistics report zero parsed files and one reused file result per indexed source file, and duplicate detection returns the same matches as before the restart

### Requirement: Changed files are parsed again

The index SHALL parse again exactly the files whose text differs from the text behind their stored result. This applies whether the change happened while no process was running or was reported by the file watcher while the index was running.

#### Scenario: File edited while the index was closed

- **GIVEN** a workspace index was built and shut down
- **WHEN** one source file is modified and a new workspace index starts for the same root
- **THEN** the index statistics report exactly one parsed file, and duplicate detection reflects the modified file's new functions

#### Scenario: Watcher change re-parses only the changed file

- **GIVEN** a running workspace index
- **WHEN** the file watcher reports one created file and one modified file in a single change set
- **THEN** the parsed-file count grows by exactly two, and duplicate detection includes the functions of both files

#### Scenario: Deleted file leaves duplicate detection

- **GIVEN** a running workspace index whose duplicate detection matches a function in file A
- **WHEN** the file watcher reports file A as removed
- **THEN** duplicate detection no longer returns matches located in file A

### Requirement: Cached results equal uncached results

Every result served from the cache SHALL equal the result of parsing the same text directly. The outputs of `index`, `get_symbol`, `get_function`, `replace_symbol` and `find_symbol_references` SHALL be identical whether or not a workspace index answered the call.

#### Scenario: Symbol tool output matches with and without the index

- **GIVEN** a workspace containing source files in several supported languages
- **WHEN** each of `index`, `get_symbol`, `get_function`, `find_symbol_references` and `replace_symbol` is called once with a ready workspace index and once without one, on identical file contents
- **THEN** each pair of calls returns identical output, and `replace_symbol` writes identical file content

### Requirement: Symbol tools use the text they read

A symbol tool SHALL resolve symbols from the file text it read during that call. It SHALL NOT use a stored result for different text, even when the file watcher has not yet reported the change.

#### Scenario: Edit not yet reported by the watcher

- **GIVEN** a ready workspace index that has served `get_symbol` for a file
- **WHEN** the file is rewritten so that the symbol moves to different lines, and `get_symbol` is called again before any watcher change set for that file is applied
- **THEN** the second call reports the symbol at its new lines

### Requirement: Unchanged files are not parsed again by tool calls

When a symbol tool supplies text that equals the text behind a stored result, the index SHALL answer from that result without parsing. A single tool call SHALL parse each file it reads at most once.

#### Scenario: Repeated lookup on an unchanged file is reused

- **GIVEN** a ready workspace index and a file that `get_symbol` has already resolved
- **WHEN** `get_symbol` is called again on the unchanged file
- **THEN** the parsed-file count is unchanged and the reused-result count grows by one

#### Scenario: Directory reference search reuses unchanged files

- **GIVEN** a ready workspace index and a directory that `find_symbol_references` has already searched
- **WHEN** `find_symbol_references` is called again on the same directory with no file changed
- **THEN** the parsed-file count is unchanged and the output equals the first call's output

### Requirement: Symbol tools fall back to direct parsing

A symbol tool SHALL parse the file directly, with identical output, when any of these holds:

- no workspace index is ready for its session;
- the file lies outside the index's workspace root;
- the index does not answer within 2 seconds;
- the index reports an error.

The tool call SHALL NOT fail because of the index.

#### Scenario: No workspace index yet

- **WHEN** `get_symbol` is called in a session whose workspace index has not finished starting
- **THEN** the call returns the same output as with a ready index

#### Scenario: File outside the workspace root

- **GIVEN** a ready workspace index
- **WHEN** `get_function` is called on a file outside the index's workspace root
- **THEN** the call succeeds with the directly parsed result and the index statistics are unchanged

#### Scenario: A busy index costs one deadline per call

- **GIVEN** a workspace index that does not reply to lookups
- **WHEN** `find_symbol_references` searches a directory with more files than one lookup batch
- **THEN** the call returns the directly parsed result after waiting for the index at most once

#### Scenario: Index does not answer in time

- **GIVEN** a workspace index that does not reply to lookups
- **WHEN** `get_symbol` is called
- **THEN** the call returns the directly parsed result within about 2 seconds plus parse time

#### Scenario: Index fails the lookup

- **GIVEN** a workspace index whose cache owner has stopped
- **WHEN** `find_symbol_references` is called
- **THEN** the call returns the directly parsed result without an error

### Requirement: Paths that cannot be keyed do not disable the cache

A source file whose path cannot be represented as a cache key (a non-UTF-8 path component) SHALL be indexed directly, and SHALL NOT prevent other files from being indexed through the cache.

#### Scenario: Non-UTF-8 file name

- **GIVEN** a workspace with valid source files and one source file whose name is not valid UTF-8
- **WHEN** the workspace index is built, shut down and started again with no file changed
- **THEN** the odd file's functions are indexed, and the second start reports zero parsed files and one reused result per other file

### Requirement: Unparseable files do not fail indexing

A file that cannot be parsed, or that cannot be indexed as text, SHALL contribute no symbols or functions, and SHALL be recorded and reused like any other per-file result. Its presence SHALL NOT prevent other files' results from being indexed or stored. This covers a file that is not UTF-8, that exceeds the size limit, or whose parser fails.

#### Scenario: One broken file among valid files

- **GIVEN** a workspace with valid source files and one file whose parser fails
- **WHEN** the workspace index is built, shut down and started again with no file changed
- **THEN** the valid files' functions are indexed both times, and the second start reports zero parsed files

### Requirement: Cache location and isolation

By default, each workspace root SHALL have its own cache directory at `<platform cache dir>/querymt/code-index/<16 hex digit hash of the canonical root path>`. Different workspace roots SHALL never share stored results.

#### Scenario: Two workspaces have separate caches

- **WHEN** workspace indexes are built for two different roots with the default storage location
- **THEN** each root's results are stored in its own directory under `querymt/code-index/`, and a result for a relative path in one root is never served for the same relative path in the other

### Requirement: Unavailable cache directory falls back to memory

If a workspace's cache directory is locked by another process, or cannot be created or opened, the index SHALL keep its results in memory for that process, log a warning, and otherwise behave the same. A directory that cannot be opened because its contents are damaged SHALL be discarded and recreated once before falling back.

#### Scenario: Cache directory held by another owner

- **GIVEN** a workspace index holding the cache directory for a root
- **WHEN** a second workspace index for the same root and cache directory is started by another owner
- **THEN** the second index starts, answers lookups and duplicate detection correctly, and logs a warning that it uses memory

#### Scenario: Damaged cache is rebuilt

- **GIVEN** a cache directory whose files have been overwritten with invalid data
- **WHEN** a workspace index starts for that root
- **THEN** the index starts, parses every file, answers correctly, and a later restart with unchanged files reports zero parsed files

#### Scenario: Recreated index in the same process keeps the disk cache

- **GIVEN** a workspace index whose cache client is still held (for example by a session) after the index manager dropped the index
- **WHEN** a new workspace index starts for the same root in the same process
- **THEN** the new index uses the same on-disk cache rather than falling back to memory, and parses no file whose result is stored

#### Scenario: Transient open failure keeps the cache

- **WHEN** a cache directory cannot be opened for a reason other than damaged contents (for example an I/O or permission error)
- **THEN** the index keeps its results in memory for that process and the directory is not deleted

### Requirement: Format or version changes discard the cache

Stored results SHALL be reused only by the same `querymt-agent-index` package version, cache schema version and extractor build (extractor sources and parser dependency versions) that wrote them. Otherwise, the whole cache for that workspace SHALL be discarded on open and rebuilt, never migrated.

#### Scenario: Schema version change rebuilds the cache

- **GIVEN** a cache written for a workspace under one schema version
- **WHEN** a workspace index for the same root opens the cache under a different schema version with no file changed
- **THEN** every indexed file is parsed again

### Requirement: Cache size is bounded

By default, a workspace cache SHALL hold at most 256 MiB of stored results and at most 200,000 entries. Results not used for 30 days SHALL be evicted. Eviction SHALL only cause recomputation, never wrong or missing answers.

#### Scenario: Caches of roots that are not opened again are removed

- **GIVEN** a cache directory for a root (for example a removed worktree) that no process holds and that was last used longer ago than the idle limit
- **WHEN** a workspace index opens a cache under the same base directory
- **THEN** that stale directory is removed, while directories that are held by another process or were used within the idle limit are kept

#### Scenario: Idle results are evicted and recomputed

- **GIVEN** a workspace index whose cache uses an idle limit shorter than the time since a file's result was last used
- **WHEN** the cache is collected and the file is looked up again with unchanged text
- **THEN** the file is parsed again and the lookup returns the same symbols as before eviction

### Requirement: Index statistics report parsing and reuse

Workspace index statistics SHALL report two cumulative counts since the index started, covering both indexing and tool lookups:

- `parsed_files`: per-file extractions that ran;
- `reused_files`: per-file results that were answered without running.

#### Scenario: Statistics after a cold build

- **WHEN** a workspace index is built for a root with no existing cache
- **THEN** `parsed_files` equals the number of indexed source files and `reused_files` is zero
