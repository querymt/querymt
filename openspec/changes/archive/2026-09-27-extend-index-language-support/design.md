# Design

## Context

The pipeline is: `index` tool (`crates/agent/src/tools/builtins/index.rs`) → `outline_index::index_file` → `get_language_for_extension()` (`outline_index/common.rs`) → `symbol_index::extractors::extract_symbols` dispatch → per-language tree-sitter extractor → `outline_projection::symbols_to_sections`. Adding a language touches exactly: the extension map, one extractor module, one dispatch arm, and (for this change only) the projection's section routing. `get_symbol`, `get_function`, `find_references`, `replace_symbol` reuse `SymbolIndex` and need no changes. The workspace already runs tree-sitter 0.26.13 (ABI range 13–16); the existing ABI-14 `tree-sitter-nix` proves older-grammar crates load fine.

## Goals / Non-Goals

**Goals:**
- Full outline support for 10 new file types with per-language tests.
- Correct whole-file line/byte coordinates for symbols extracted from embedded `<script>` content.
- Additive projection change; existing languages' outline output unchanged.

**Non-Goals:**
- Vue support (crates.io grammar is abandoned; not low-hanging).
- Feature-flagging grammar crates behind cargo features.
- Markdown inline constructs (links, emphasis), YAML anchors/aliases, JSON root scalars beyond a minimal fallback.
- Changes to tool result formats other than new section names.

## Decisions

### D1: Grammar crate selection

Use `tree-sitter-svelte-ng 1.0.2` (maintained tree-sitter-grammars fork) — the older `tree-sitter-svelte 0.10.2` (2022) predates snippet statements and current Svelte syntax. Use official crates where they exist: `tree-sitter-bash 0.25.1`, `tree-sitter-php 0.24.2`, `tree-sitter-json 0.24.8`. Community crates otherwise: `tree-sitter-swift 0.7.3`, `tree-sitter-yaml 0.7.2`. For kotlin/toml/markdown, the original crates.io pins (`tree-sitter-kotlin 0.3.5`, `tree-sitter-toml 0.20.0`, `tree-sitter-markdown 0.7.1`) drag in tree-sitter runtime crates 0.19/0.20 whose build scripts link a second `libtree-sitter` globally; the linker then resolves `ts_*` symbols to the old runtime and every ABI-14 language fails with `parse returned None`. Swap to maintained grammars that bind via `tree-sitter-language` only (same pattern as svelte-ng): `tree-sitter-kotlin` from git `fwcd/tree-sitter-kotlin` master (0.4.0, pinned to rev `1852ea17b7f60fb3f9d84e0b1555d56b46b39fb1`), `tree-sitter-toml-ng 0.7.0`, and `tree-sitter-md 0.5.3` (tree-sitter-grammars). `tree-sitter-md 0.5.3` depends on tree-sitter ^0.26, which unifies with the runtime already in the graph. Alternative considered: git dependencies for maximum freshness — rejected for crates.io-available grammars; crates.io pins are reproducible and match existing practice.

### D2: Svelte = host grammar walk + TS re-parse of script bodies

Parse the whole file with the svelte grammar, walk `source_file` children:
- `script_element`: detect `context="module"` attribute; take the `raw_text` child, slice via `safe_slice`, re-parse with `tree_sitter_typescript::LANGUAGE_TYPESCRIPT` (a superset that also parses plain JS — same trick the existing typescript extractor uses for `javascript`), and reuse `typescript::extract()`. Then offset every returned symbol recursively: `start_line/end_line += raw_text.start_position().row`, `start_byte/end_byte += raw_text.start_byte()`. Digests are content-relative hashes and are left untouched.
- `element` (top level only, no recursion): markup entry `<Tag attr1 attr2 …>` (names only, no values).
- `snippet_statement` → `SymbolKind::Function`, signature `{#snippet name(args)}`.
- `if_statement` / `each_statement` / `await_statement` / `key_statement` → markup entry, first-line signature (`{#if cond}`).
- `style_element` → markup entry `<style>`.
- `text`, `comment`, `expression`, `@const/@debug/@render` tags → skipped.

Alternatives considered: (a) single-grammar regex extraction — rejected, inconsistent with the tree-sitter architecture and fragile for snippets/blocks; (b) two-pass parsing of the whole file as TS — impossible, markup breaks the TS grammar.

### D3: Markup/sections routing via signature prefixes on Module kind

No new `SymbolKind` variants. New extractors emit `SymbolKind::Module` with signature prefixes, and `outline_projection` routes them (checked **after** the existing `namespace `/`module `/`defmodule `/nix checks so existing languages are unaffected):
- `{#` or `<` → new `markup` section (svelte blocks, elements, `<style>`)
- `#` or `[` → new `sections` section (markdown headings, TOML `[table]`/`[[array]]`)

Section output order: `sections` after `modules`; `markup` just before `tests`. Config keys (JSON/YAML/TOML) emit `SymbolKind::Const` → existing `constants` section, so no routing change for them. Alternative considered: new `SymbolKind::Key`/`Section` variants — rejected as more invasive (enum, `as_str`, `FromStr`, kind filters) for no observable benefit.

### D4: Per-language extractor mapping (existing extractor pattern)

- **bash**: `function_definition` → Function (`test_*` names → Test), top-level `variable_assignment` → Const, `source`/`. file` commands → Import.
- **php**: `namespace_definition` → Module, `class/interface/trait/enum_declaration`, `function_definition`/`method_declaration`, `use_declaration` → Import, `const_declaration` → Const. Use the crate's PHP-only language constant (not the HTML-embedded variant) for typical code files; validate the exact export name at implementation.
- **kotlin**: `class_declaration`/`object_declaration` → Class, `interface_declaration` → Interface, `function_declaration` → Function/Method, top-level `val`/`var` → Const, `typealias` → TypeAlias.
- **swift**: `function_declaration` → Function, class/struct declarations, `enum` → Enum, `protocol` → Trait, `extension` → Impl, `property_declaration` → Const, `import_statement` → Import, `typealias` → TypeAlias.
- **json**: top-level object `pair`s → Const (`key: <first line of value>`), nested pairs as children.
- **yaml**: top-level `block_mapping_pair` keys → Const; nested mappings as children; sequences collapsed into signature (`key: [N items]`).
- **toml**: `table`/`table_array_element` headers → Module (sections routing); contained pairs → Const children; root pairs → Const.
- **markdown**: `atx_heading`/setext headings → Module (`#` routing), heading text as signature, nested `section`s as children.

Community-grammar node-kind names (kotlin, swift, yaml, toml) must be verified against each crate's actual `node-types.json` during implementation; a throwaway parse-dump test per language is the cheap way. This is validated per language before its extractor is considered done.

**Node kinds verified via throwaway parse-dump (task 2.1):**
- bash: `function_definition` (name = first `word` child), `variable_assignment` (`variable_name`), `command` + `command_name` (`source`/`.` → Import).
- php: `namespace_definition`/`namespace_name`, `namespace_use_declaration` (namespace-level) and `use_declaration`, `const_declaration`/`const_element`/`name`, `class_declaration`, `interface_declaration`, `trait_declaration`, `enum_declaration`, `method_declaration`, `function_definition`, `enum_case`.
- kotlin: `import_list`/`import_header`, `property_declaration` (`const` via `modifiers` > `property_modifier`; name via `variable_declaration` > `simple_identifier`), `type_alias`, `class_declaration` (also parses `interface` and `enum class` — keyword check needed), `object_declaration`, `function_declaration` (`simple_identifier`, `function_body`).
- swift: `import_declaration`, `property_declaration` (`value_binding_pattern`, name via `pattern` > `simple_identifier`), `class_declaration` (also parses `struct`/`enum`/`extension`/`actor` — keyword check needed; body `class_body`/`enum_class_body`), `protocol_declaration` (`protocol_function_declaration`), `typealias_declaration`, `function_declaration` (`function_body`).
- json: `document` > `object` > `pair` (`document` has no field names — first named child is the root value; `pair` has `key`/`value` fields); yaml: `stream` > `document` > `block_node` > `block_mapping` > `block_mapping_pair` (`key`/`value` fields; values are wrapped in `block_node`/`flow_node`); toml (toml-ng): `pair`, `table`, `table_array_element`, all without field names (positional children; keys via `bare_key`/`dotted_key`); markdown (tree-sitter-md): `document` > `section` (nested by heading level) containing `atx_heading`/`setext_heading` with a `heading_content` field; setext headings do not create their own `section` (they appear inside the enclosing section).
- svelte: `script_element` (`start_tag` attributes for `context="module"`, `raw_text` child), `element` (`self_closing_tag`/`start_tag` with `tag_name`/`attribute`), `if_statement`/`each_statement` (`*_start` with `svelte_raw_text`), `snippet_statement` > `snippet_start` > `snippet_name`, `style_element` — as per D2.

### D5: Tool strings

`index.rs`: extend `definition().description` and the `OutlineError::UnsupportedLanguage` error extension list. The existing `test_index_unsupported_extension` assertions stay valid (message still contains `ex` and `lua`); extend it to assert the new extensions too.

## Risks / Trade-offs

- [Compile time / binary size: 9 new generated C parsers (bash's alone is ~350k LOC)] → accepted for now; all officially supported profiles compile them once. Revisit a cargo feature gate only if CI build time regresses materially.
- [Community grammar drift/quality (kotlin, swift, yaml, toml)] → per-language extraction tests pin expected node kinds; if a crate is broken at resolve/build time, swap to a git dep of its maintained fork (precedent: the similarity fork pin in `Cargo.toml`).
- [Svelte offset math could silently corrupt `replace_symbol` byte ranges] → dedicated tests assert absolute line numbers and that slicing the file by a symbol's byte range yields its declaration; add a `replace_symbol` round-trip on a `.svelte` function as an integration check.
- [Older-ABI grammars (yaml 0.7, kotlin 0.3, swift 0.7) may still fail `set_language` at runtime if their ABI < 13] → verified: all listed crates generate ABI ≥ 14; each extractor test exercises `set_language` implicitly, failing loudly if not.
- [Markdown inline syntax produces ERROR nodes in the block-only grammar] → harmless: only heading nodes are read; inline text never surfaces.

## Migration Plan

No data or API migration. Land in per-language commits: plumbing (Cargo.toml + extension map) first, then code-language extractors, then svelte, then config formats + projection routing, then tool strings. Rollback is reverting the corresponding commits; no persisted state depends on the new sections.

## Open Questions

(None — grammar crate choices and projection routing were resolved during planning.)
