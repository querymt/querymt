# Spec Delta

## Purpose

Defines the structural-outline contract for the agent's `index` tool and the shared symbol-extraction pipeline behind it: which file types are indexed, what outline content each type yields, the section layout of the outline, and coordinate correctness guarantees that downstream symbol tools depend on.

## ADDED Requirements

### Requirement: Outline extraction for code languages

The `index` tool SHALL produce a structural outline (path, language, and non-empty sections with line ranges) for files with extensions: `rs`, `py`, `pyi`, `ts`, `tsx`, `js`, `jsx`, `mjs`, `cjs`, `go`, `java`, `c`, `h`, `cpp`, `cc`, `cxx`, `hpp`, `hxx`, `c++`, `cs`, `rb`, `ex`, `exs`, `nix`, `lua`, `sh`, `bash`, `php`, `kt`, `kts`, `swift`, `svelte`, `json`, `yaml`, `yml`, `toml`, `md`, `markdown`.

#### Scenario: Bash script outline

- **WHEN** the `index` tool is called on a `.sh` file containing shell functions, variable assignments, and `source` lines
- **THEN** the outline includes functions, constants, and imports sections with valid line ranges, and reports `language: bash`

#### Scenario: PHP file outline

- **WHEN** the `index` tool is called on a `.php` file with a namespace, classes, and functions
- **THEN** the outline includes modules, classes, and functions sections with valid line ranges, and reports `language: php`

#### Scenario: Kotlin file outline

- **WHEN** the `index` tool is called on a `.kt` file with classes, interfaces, and top-level functions
- **THEN** the outline includes classes, interfaces, and functions sections with valid line ranges, and reports `language: kotlin`

#### Scenario: Swift file outline

- **WHEN** the `index` tool is called on a `.swift` file with classes, structs, protocols, extensions, and functions
- **THEN** the outline includes the corresponding sections with valid line ranges, and reports `language: swift`

### Requirement: Svelte outline includes script symbols and template structure

The `index` tool SHALL index `.svelte` files such that the outline contains both symbols from embedded `<script>` content (imports, functions, constants, classes) and compact `markup` entries for top-level template structure (elements, `{#if}`/`{#each}`/`{#await}`/`{#key}` blocks, `<style>`), with `{#snippet}` declarations reported as functions.

#### Scenario: Svelte file with script and template

- **WHEN** the `index` tool is called on a `.svelte` file whose `<script>` imports a component, declares a function and a reactive variable, and whose template has a top-level element, an `{#if}` block, and a `{#snippet}`
- **THEN** the outline reports `language: svelte`, lists the import under imports, the function and snippet under functions, the variable under constants, and the element and `{#if}` block under markup

#### Scenario: Svelte module script

- **WHEN** a `.svelte` file contains `<script context="module">` declarations
- **THEN** symbols declared in the module script appear in the outline with the same section mapping as instance-script symbols

### Requirement: Embedded-script coordinates are whole-file coordinates

For symbols extracted from embedded script content (e.g. `<script>` in a `.svelte` file), the reported start/end line numbers and byte offsets SHALL refer to positions in the containing file, not the embedded snippet, so that downstream tools (`get_function`, `get_symbol`, `replace_symbol`, `find_references`) operate on correct ranges.

#### Scenario: Script symbol line numbers are absolute

- **WHEN** a `.svelte` file has its `<script>` tag starting on line 2 and a function declared inside it
- **THEN** the function's outline entry shows a start line greater than 2 corresponding to its actual file position

#### Scenario: Byte range slices the declaration

- **WHEN** a symbol extracted from `.svelte` embedded script content has its reported byte range sliced from the file content
- **THEN** the sliced text contains the symbol's declaration

### Requirement: Config and documentation outlines use sections and constants

The `index` tool SHALL index `.json`, `.yaml`, `.yml`, `.toml`, `.md`, and `.markdown` files by mapping scalar keys to `constants` entries and container headings (Markdown headings, TOML `[table]` and `[[array]]` headers) to a new `sections` outline section, with nested structure represented as children.

#### Scenario: Markdown headings become sections

- **WHEN** the `index` tool is called on a `.md` file with `#`/`##` headings
- **THEN** the outline reports `language: markdown` and lists each heading under `sections` with its heading text and line range

#### Scenario: TOML tables and keys

- **WHEN** the `index` tool is called on a `.toml` file with a `[server]` table containing key/value pairs
- **THEN** the `[server]` header appears under `sections` with the keys as children under `constants` entries

#### Scenario: JSON object keys

- **WHEN** the `index` tool is called on a `.json` file with a top-level object
- **THEN** each top-level key appears under `constants` with nested keys as children

#### Scenario: YAML mapping keys

- **WHEN** the `index` tool is called on a `.yaml` file with a top-level mapping
- **THEN** each top-level key appears under `constants` with nested mappings as children

### Requirement: New outline sections do not alter existing languages

The `markup` and `sections` outline sections introduced by this change SHALL only be produced for the new file types; outlines for previously supported languages SHALL be byte-identical to their output before this change.

#### Scenario: Existing language output unchanged

- **WHEN** the `index` tool is called on a Rust, Python, TypeScript, Go, Java, C, C++, C#, Ruby, Elixir, Nix, or Lua file
- **THEN** the outline contains no `markup` or `sections` sections and matches the previously established section layout for that language

### Requirement: Tool contract documents and reports supported types

The `index` tool's description SHALL enumerate the supported languages, and calls on unsupported extensions SHALL fail with an error that names the requested extension and lists the supported extensions, including the newly added ones.

#### Scenario: Unsupported extension error is actionable

- **WHEN** the `index` tool is called on a file with an unsupported extension (e.g. `.xyz`)
- **THEN** the error message contains the unsupported extension and the full list of supported extensions including `svelte`, `sh`, `php`, `kt`, `swift`, `json`, `yaml`, `toml`, and `md`

#### Scenario: Tool description lists new languages

- **WHEN** the `index` tool definition is inspected
- **THEN** its description mentions Svelte, Bash, PHP, Kotlin, Swift, JSON, YAML, TOML, and Markdown
