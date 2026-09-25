# Proposal

## Why

The `index` tool produces structural skeletons only for 13 languages. Svelte files — first-class in this repository's own UI stack — are rejected outright, and other common file types (shell scripts, config files, docs) fall back to full-file reads, wasting tokens on reconnaissance before any targeted read.

## What Changes

- Add Svelte (`.svelte`) support to the outline extraction pipeline: script contents are indexed as symbols (imports, functions, constants, classes) and top-level template structure is surfaced as compact `markup` entries (`<Tag>`, `{#if}`, `{#each}`, `{#snippet}`).
- Add Bash (`.sh`, `.bash`), PHP (`.php`), Kotlin (`.kt`, `.kts`), and Swift (`.swift`) extractors following the existing per-language tree-sitter pattern.
- Add config/doc formats with a new projection mapping: JSON, YAML, TOML (keys → `constants`, table/heading containers → new `sections` section) and Markdown (headings → `sections`).
- Introduce two new outline sections, `markup` and `sections`, produced by routing `Module`-kind symbols by signature prefix; existing languages' output is unchanged.
- Extend the extension→language map (`get_language_for_extension`) and the `index` tool description / unsupported-extension error message.
- All downstream symbol tools (`get_symbol`, `get_function`, `find_references`, `replace_symbol`) gain the same file-type support automatically via the shared `SymbolIndex` pipeline. Byte/line offsets for symbols extracted from embedded `<script>` content in `.svelte` files are corrected to whole-file coordinates so `replace_symbol` remains safe.

## Capabilities

### New Capabilities

- `index-tool`: Structural outline extraction for the agent's `index` tool (and the shared symbol-extraction pipeline behind it): supported file types, per-type outline content, section layout, and offset correctness guarantees.

### Modified Capabilities

(none — no existing spec covers the index tool or outline extraction)

## Impact

- **Code**: `crates/agent/src/index/outline_index/common.rs` (extension map), `crates/agent/src/index/outline_index/outline_projection.rs` (new sections), new extractor modules under `crates/agent/src/index/symbol_index/extractors/` (`svelte.rs`, `bash.rs`, `kotlin.rs`, `swift.rs`, `php.rs`, `json.rs`, `yaml.rs`, `toml.rs`, `markdown.rs`), dispatch in `symbol_index/extractors/mod.rs`, tool strings in `crates/agent/src/tools/builtins/index.rs`.
- **Dependencies**: 9 new tree-sitter grammar crates in `crates/agent/Cargo.toml` (`tree-sitter-svelte-ng`, `tree-sitter-bash`, `tree-sitter-php`, `tree-sitter-kotlin`, `tree-sitter-swift`, `tree-sitter-json`, `tree-sitter-yaml`, `tree-sitter-toml`, `tree-sitter-markdown`). All are ABI-compatible with the tree-sitter 0.26 runtime already in use. Expect increased compile time and binary size from new generated C parsers.
- **Behavior**: `index`, `get_symbol`, `get_function`, `find_references`, `replace_symbol` accept the new file types; no existing-language output changes.
