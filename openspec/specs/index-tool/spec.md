# index-tool Specification

## Purpose

Defines the structural-outline contract for the agent's `index` tool and the shared symbol-extraction pipeline behind it: which file types are indexed, what outline content each type yields, the section layout of the outline, and coordinate correctness guarantees that downstream symbol tools depend on.

## Requirements

### Requirement: Outline extraction for code languages

The `index` tool SHALL produce a structural outline (path, language, and non-empty sections with line ranges) for files with extensions: `rs`, `py`, `pyi`, `ts`, `tsx`, `js`, `jsx`, `mjs`, `cjs`, `go`, `java`, `c`, `h`, `cpp`, `cc`, `cxx`, `hpp`, `hxx`, `c++`, `cs`, `rb`, `ex`, `exs`, `nix`, `lua`, `sh`, `bash`, `php`, `kt`, `kts`, `swift`, `lean`, `svelte`, `json`, `yaml`, `yml`, `toml`, `md`, `markdown`.

#### Scenario: Bash script outline

- **WHEN** the `index` tool is called on a `.sh` file containing shell functions, variable assignments, and `source` lines
- **THEN** the outline includes functions, constants, and imports sections with valid line ranges, and reports `language: bash`

#### Scenario: PHP file outline

- **WHEN** the `index` tool is called on a `.php` file with a namespace, classes, and functions
- **THEN** the outline includes namespaces, classes, and functions sections with valid line ranges, and reports `language: php`

#### Scenario: Kotlin file outline

- **WHEN** the `index` tool is called on a `.kt` file with classes, interfaces, and top-level functions
- **THEN** the outline includes classes, interfaces, and functions sections with valid line ranges, and reports `language: kotlin`

#### Scenario: Swift file outline

- **WHEN** the `index` tool is called on a `.swift` file with classes, structs, protocols, extensions, and functions
- **THEN** the outline includes the corresponding sections with valid line ranges, and reports `language: swift`

#### Scenario: Lean file outline

- **WHEN** the `index` tool is called on a `.lean` file with imports, a top-level `def`, a top-level `theorem`, and a `structure`
- **THEN** the outline includes imports, functions, theorems, and types sections with valid line ranges, and reports `language: lean`

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

The `markup`, `sections`, and `theorems` outline sections SHALL only be produced for the file types that introduced them (`markup` and `sections` for Svelte, JSON, YAML, TOML and Markdown; `theorems` for Lean); outlines for previously supported languages SHALL be byte-identical to their output before these sections were introduced.

#### Scenario: Existing language output unchanged

- **WHEN** the `index` tool is called on a Rust, Python, TypeScript, Go, Java, C, C++, C#, Ruby, Elixir, Nix, or Lua file
- **THEN** the outline contains no `markup` or `sections` sections and matches the previously established section layout for that language

#### Scenario: Theorems section is Lean-only

- **WHEN** the `index` tool is called on a non-Lean file (e.g. Rust, PHP, C#, Kotlin, or Swift) whose content includes the word `theorem` or `lemma`, or that contains namespaces
- **THEN** the outline contains no `theorems` section and namespace members are projected exactly as before this change

### Requirement: Tool contract documents and reports supported types

The `index` tool's description SHALL enumerate the supported languages, and calls on unsupported extensions SHALL fail with an error that names the requested extension and lists the supported extensions, including the newly added ones.

#### Scenario: Unsupported extension error is actionable

- **WHEN** the `index` tool is called on a file with an unsupported extension (e.g. `.xyz`)
- **THEN** the error message contains the unsupported extension and the full list of supported extensions including `svelte`, `sh`, `php`, `kt`, `swift`, `lean`, `json`, `yaml`, `toml`, and `md`

#### Scenario: Tool description lists new languages

- **WHEN** the `index` tool definition is inspected
- **THEN** its description mentions Svelte, Bash, PHP, Kotlin, Swift, Lean, JSON, YAML, TOML, and Markdown

### Requirement: Lean declarations map to outline sections

For `.lean` files, the `index` tool SHALL report each top-level declaration under the outline section matching its kind. Labels SHALL show the declaration header, including modifiers such as `private` or `noncomputable` but excluding `@[...]` attributes.

#### Scenario: Lean imports and opens are imports

- **WHEN** a `.lean` file contains `import Mathlib.Data.Nat.Basic` and `open Nat`
- **THEN** both appear under `imports` with their line numbers

#### Scenario: Lean definitions are functions

- **WHEN** a `.lean` file contains top-level `def`, `abbrev`, and `opaque` declarations
- **THEN** each appears under `functions` labelled with its header line

#### Scenario: Lean types and instances

- **WHEN** a `.lean` file contains a top-level `structure Point` with fields `x` and `y`, a `class Shape` with a field, an `inductive Color` with constructors `red` and `green`, and an `instance : ToString Point`
- **THEN** `Point` appears under `types` with children `x` and `y`, `Shape` appears under `traits` with its field as a child, `Color` appears under `types` with children `red` and `green`, and the instance appears under `impls`

#### Scenario: Lean axioms and constants

- **WHEN** a `.lean` file contains top-level `axiom` and `constant` declarations
- **THEN** each appears under `constants`

#### Scenario: Lean examples are tests

- **WHEN** a `.lean` file contains an `example` declaration and the `index` tool is called with `include_tests: false`
- **THEN** the example is omitted, and with the default `include_tests: true` it appears under `tests`

#### Scenario: Declarations behind an `in` prefix are indexed

- **WHEN** a `.lean` file contains `set_option maxHeartbeats 400000 in`, `open Classical in` or `variable (n : Nat) in` directly followed by a `theorem` or `def`
- **THEN** that declaration appears in the outline with the range of the declaration itself, and the `open … in` prefix is not listed as an import

#### Scenario: Lean macro commands are macros

- **WHEN** a `.lean` file contains top-level `macro`, `syntax`, `notation`, and `macro_rules` commands
- **THEN** each appears under `macros`

#### Scenario: Attributed declaration label and range

- **WHEN** a `.lean` file contains `@[simp] private theorem foo : 1 = 1 := rfl` spanning lines 3–4 with the attribute on line 3
- **THEN** the outline entry is labelled starting with `private theorem foo` and its line range starts at line 3

### Requirement: Lean theorems section

For `.lean` files, the `index` tool SHALL list top-level `theorem` and `lemma` declarations, including those with modifiers or attributes, in a `theorems` outline section placed directly after `functions`, and SHALL NOT also list them under `functions`.

#### Scenario: Top-level theorem and lemma

- **WHEN** a `.lean` file contains a top-level `theorem add_zero'` and a top-level `lemma zero_add'` outside any namespace
- **THEN** both appear under `theorems` and neither appears under `functions`

### Requirement: Lean namespaces nest their declarations

For `.lean` files, each `namespace` SHALL appear once under `namespaces`. Its range runs from the `namespace` line to the matching `end`, or to the last line of the file if it is never closed. The declarations it contains SHALL be listed as its children and SHALL NOT also appear in top-level outline sections.

#### Scenario: Declarations nest under namespace

- **WHEN** a `.lean` file contains `namespace Geometry` holding a `structure Point`, a `def dist`, and a `theorem dist_self`, followed by `end Geometry`
- **THEN** `namespaces` lists `namespace Geometry` with a line range ending at the `end Geometry` line, and its children are `Point`, `dist`, and `dist_self` in source order, with none of them in `types`, `functions`, or `theorems`

#### Scenario: Nested namespaces nest further

- **WHEN** a `.lean` file contains `namespace A`, then `namespace B` with `def f`, then `end B`, then `end A`
- **THEN** `namespace A` has the child `namespace B`, which has the child `f`

#### Scenario: Unclosed namespace runs to end of file

- **WHEN** a `.lean` file opens `namespace Foo` and never closes it
- **THEN** the `namespace Foo` entry's range ends at the last line of the file and contains every declaration after it

#### Scenario: Sections and mutual blocks are transparent

- **WHEN** a `.lean` file contains `namespace Foo`, then `section Bar` with `def a`, then `end Bar`, then `mutual` with `def b` and `def c`, then `end`, then `def d`, then `end Foo`
- **THEN** `a`, `b`, `c`, and `d` are all direct children of `namespace Foo`, no entry is emitted for the section or the mutual block, and the `end` closing the mutual block does not close the namespace

#### Scenario: Dotted namespace names open one scope per component

- **WHEN** a `.lean` file contains `namespace A.B`, then `def x`, then `end B`, then `def y`, then `end A`
- **THEN** a single `namespace A.B` entry is listed, its range ends at `end A`, `x` has qualified name `A.B.x`, `y` has qualified name `A.y`, and declarations after `end A` are top-level

#### Scenario: Dotted end closes several namespaces

- **WHEN** a `.lean` file contains `namespace A`, then `namespace B`, then `end A.B`
- **THEN** both namespaces are closed and later declarations are top-level

#### Scenario: Public section is closed by end

- **WHEN** a `.lean` file contains `namespace Foo`, then `public section`, then `end`, then `def b`, then `end Foo`
- **THEN** the unnamed `end` closes the section, and `b` is a child of `namespace Foo`

#### Scenario: Imports and opens stay at top level

- **WHEN** a `.lean` file contains `open Nat` inside `namespace Foo`
- **THEN** the `open` appears under `imports`, not as a child of `namespace Foo`

#### Scenario: Re-opened namespace yields separate entries

- **WHEN** a `.lean` file contains `namespace Foo … end Foo` twice with different declarations
- **THEN** `namespaces` lists two `namespace Foo` entries, each with its own declarations and line range

### Requirement: Lean symbols are addressable by dotted qualified name

For `.lean` files, symbol lookups (`get_symbol`, `get_function`, `replace_symbol`, `find_symbol_references`) SHALL accept both a declaration's own name and its qualified name. The qualified name joins the enclosing namespace names and the declaration name with `.`, matching Lean's naming. The symbol's byte range SHALL cover the whole declaration, including its attributes and modifiers.

#### Scenario: Lookup by qualified name

- **WHEN** `get_symbol` is called for `Geometry.dist` on a `.lean` file where `def dist` is declared inside `namespace Geometry`
- **THEN** the `dist` declaration is returned

#### Scenario: Dotted declaration name inside namespace

- **WHEN** `def Point.norm` is declared inside `namespace Geometry`
- **THEN** the symbol's name is `Point.norm` and its qualified name is `Geometry.Point.norm`

#### Scenario: `_root_` names ignore enclosing namespaces

- **WHEN** `def _root_.Foo.bar` is declared inside `namespace X`
- **THEN** its qualified name is `Foo.bar`

#### Scenario: Replace covers attributes

- **WHEN** `replace_symbol` replaces a Lean declaration written as `@[simp] theorem foo …`
- **THEN** the replaced range includes the `@[simp]` attribute, so no orphaned attribute remains

### Requirement: Lean extraction tolerates unsupported syntax

Lean files whose syntax the parser does not fully recognize (custom notation, unsupported tactics) SHALL still produce an outline containing every declaration the parser recognizes, including declarations inside regions the parser marks as malformed, instead of failing the `index` call.

#### Scenario: Partial parse still yields declarations

- **WHEN** a `.lean` file contains a declaration using syntax the grammar cannot parse, the parser confines the error to a malformed region, and a well-formed `def g` follows that the parser recognizes as a declaration
- **THEN** the `index` call succeeds and `g` appears in the outline with its correct line range

#### Scenario: Declaration absorbed by a malformed block is out of scope

- **WHEN** an ill-formed tactic block causes the parser to absorb the following `def g` into the preceding declaration's parse tree
- **THEN** the `index` call still succeeds, and `g` is not required to appear because no declaration node exists to recover
