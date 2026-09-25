# Tasks

## 1. Plumbing: dependencies and extension map

- [ ] 1.1 Add the 9 grammar crates to `crates/agent/Cargo.toml` under the tree-sitter block (`tree-sitter-svelte-ng`, `tree-sitter-bash`, `tree-sitter-php`, `tree-sitter-kotlin`, `tree-sitter-swift`, `tree-sitter-json`, `tree-sitter-yaml`, `tree-sitter-toml`, `tree-sitter-markdown`) and run `cargo check -p querymt-agent` to verify resolution and C compilation succeed
- [ ] 1.2 Extend `get_language_for_extension` in `crates/agent/src/index/outline_index/common.rs` with the new extensions (`svelte`, `sh`/`bash`, `php`, `kt`/`kts`, `swift`, `json`, `yaml`/`yml`, `toml`, `md`/`markdown`) and extend the mapping test in `outline_index/tests.rs`; verify with `cargo test -p querymt-agent outline_index`

## 2. Code-language extractors (bash, php, kotlin, swift)

- [ ] 2.1 Dump each grammar's node kinds for the constructs listed in design D4 (one throwaway parse-dump test per language) and record confirmed node-kind names; delete the dumps after recording
- [ ] 2.2 Implement `bash.rs` extractor (functions, `test_*` → Test, top-level assignments → Const, `source` → Import), add dispatch arm in `symbol_index/extractors/mod.rs`, and add an extraction test asserting kinds and line ranges; verify with `cargo test -p querymt-agent bash`
- [ ] 2.3 Implement `php.rs` extractor (namespace → Module, class/interface/trait/enum, functions/methods, `use` → Import, const → Const) with the crate's PHP-only language constant, add dispatch arm, and add an extraction test; verify with `cargo test -p querymt-agent php`
- [ ] 2.4 Implement `kotlin.rs` extractor (class/object → Class, interface → Interface, fun → Function/Method, top-level val → Const, typealias → TypeAlias), add dispatch arm, and add an extraction test; verify with `cargo test -p querymt-agent kotlin`
- [ ] 2.5 Implement `swift.rs` extractor (func → Function, class/struct, enum → Enum, protocol → Trait, extension → Impl, properties → Const, import → Import), add dispatch arm, and add an extraction test; verify with `cargo test -p querymt-agent swift`

## 3. Svelte extractor

- [ ] 3.1 Implement `svelte.rs`: host-grammar walk (elements, blocks, snippets, style per design D2) with `script_element` re-parsed via `typescript::extract` and recursive line/byte offsetting back to whole-file coordinates; add dispatch arm; verify with `cargo check -p querymt-agent`
- [ ] 3.2 Add svelte extraction tests: imports/functions/constants from `<script>`, module-script symbols, `{#snippet}` as Function, top-level element and `{#if}` as markup-prefixed Module signatures, and an absolute-coordinate test asserting a script symbol's start line matches its file position and slicing the file by its byte range yields its declaration; verify with `cargo test -p querymt-agent svelte`

## 4. Config formats and projection routing

- [ ] 4.1 Extend `outline_projection.rs` with `sections` and `markup` buckets routed by Module signature prefixes (`#`/`[` → sections, `{#`/`<` → markup) after the existing prefix checks, placed `sections` after `modules` and `markup` before `tests`; add a regression test asserting existing-language outlines contain no `markup`/`sections`; verify with `cargo test -p querymt-agent outline_projection`
- [ ] 4.2 Implement `json.rs` (top-level object keys → Const with nested children), add dispatch arm, and add an extraction test; verify with `cargo test -p querymt-agent json`
- [ ] 4.3 Implement `yaml.rs` (top-level mapping keys → Const, nested mappings as children, sequences collapsed in signature), add dispatch arm, and add an extraction test; verify with `cargo test -p querymt-agent yaml`
- [ ] 4.4 Implement `toml.rs` (`[table]`/`[[array]]` → sections-routed Module, keys → Const children, root pairs → Const), add dispatch arm, and add an extraction test; verify with `cargo test -p querymt-agent toml`
- [ ] 4.5 Implement `markdown.rs` (ATX/setext headings → sections-routed Module with heading-text signatures, nested sections as children), add dispatch arm, and add an extraction test; verify with `cargo test -p querymt-agent markdown`

## 5. Tool contract strings

- [ ] 5.1 Update `index.rs` `definition().description` to enumerate Svelte, Bash, PHP, Kotlin, Swift, JSON, YAML, TOML, Markdown and update the `UnsupportedLanguage` error extension list; add a description test and extend `test_index_unsupported_extension` to assert `svelte` appears; verify with `cargo test -p querymt-agent index`

## 6. Integration checks

- [ ] 6.1 Add an end-to-end `index` tool test on a `.svelte` fixture (imports + functions + markup sections present, `language: svelte`) and a `replace_symbol` round-trip test replacing a function declared in `.svelte` embedded script content, verifying the file content after replacement; verify with `cargo test -p querymt-agent replace_symbol`
- [ ] 6.2 Run the full gate per AGENTS.md: `cargo clippy -p querymt-agent --all-targets --features dashboard,oauth -- -D warnings`, `cargo fmt --all`, `cargo test -p querymt-agent`; verify all pass
