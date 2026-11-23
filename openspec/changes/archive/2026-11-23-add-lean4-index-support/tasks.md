# Tasks

## 1. Grammar dependency

- [x] 1.1 Record a baseline build time. Run `cargo clean -p querymt-agent-index && cargo build -p querymt-agent-index --timings` and note the wall time and the `querymt-agent-index` unit time for the PR description.
  - Recorded: baseline `cargo build -p querymt-agent-index --timings` took 51.0s wall (the `cargo clean -p` removed 0 files, so it also built uncached dependencies; the `querymt-agent-index` unit itself took 1.73s).
- [x] 1.2 Add the grammar to the tree-sitter block of `crates/agent-index/Cargo.toml` as `tree-sitter-lean = { git = "https://github.com/Julian/tree-sitter-lean", rev = "fefc2eff9a83eeb72cda44646709e40e33e09175" }`. Above it, add a comment explaining why: the crates.io `tree-sitter-lean4` crate needs tree-sitter 0.25, which conflicts with 0.26 through `links`. Verify with `cargo check -p querymt-agent-index`: it resolves, compiles `parser.c` and `scanner.c` without a tree-sitter CLI, and `Cargo.lock` gains `tree-sitter-lean` and no second `tree-sitter` version.
  - Verified: `cargo check -p querymt-agent-index` resolved and compiled the grammar without a tree-sitter CLI. `Cargo.lock` gained only `tree-sitter-lean 0.2.0` (deps `cc`, `tree-sitter-language`); no second `tree-sitter` version.
- [x] 1.3 Regenerate the workspace-hack with `cargo hakari generate && cargo hakari manage-deps`. Verify `cargo hakari verify` passes.
  - Verified: `cargo hakari generate` and `manage-deps` reported no changes; `cargo hakari verify` passed.
- [x] 1.4 Repeat the 1.1 timing run with the grammar and record the difference next to the baseline (design D1 risk).
  - Recorded: after `cargo clean -p querymt-agent-index -p tree-sitter-lean`, `cargo build -p querymt-agent-index --timings` took 2.7s wall, including compiling `tree-sitter-lean`. This is not comparable with 1.1 because the 1.1 run also built dependencies. The grammar costs under 3s in the dev profile; release/CI numbers are not measured.

## 2. Extension mapping and dispatch

- [x] 2.1 Add `"lean" => Some("lean")` to `get_language_for_extension` in `crates/agent-index/src/outline_index/common.rs`. Add a `"lean" => lean::extract(source)` arm and `pub mod lean;` to `crates/agent-index/src/symbol_index/extractors/mod.rs`; `lean.rs` starts as a stub that returns an empty vec. Extend `extension_mapping_includes_new_languages` in `outline_index/tests.rs` to assert `lean → Some("lean")`. Verify with `cargo nextest run -p querymt-agent-index -E 'test(/extension_mapping_includes_new_languages/)'`.
  - Verified: `cargo nextest run -p querymt-agent-index -E 'test(/extension_mapping_includes_new_languages/)'` 1 passed.

## 3. Lean extractor

- [x] 3.1 Add a throwaway parse-dump test that prints the S-expression of a Lean fixture covering every construct in design D3, then delete it. From its output, record in design D3 the confirmed node kinds and field names: `module`, `import`, `open`, `export`, `namespace`, `section`, `public_section`, `mutual`, `end`, `declaration`, `attributes`, `def`, `theorem`, `abbrev`, `opaque`, `instance`, `structure`, `field`, `ctor`, `inductive`, `ctor_alt`, `axiom`, `constant`, `example`, `where_decls`, `where_aux_def`, `where_struct`, `struct_field`, the macro command kinds, and how `ERROR` wraps an unparsable declaration. Done when D3 lists the confirmed names, with any mismatch corrected.
  - Done: parse-dump test run against fixtures and removed; confirmed names and three mismatches recorded in design D3.
- [x] 3.2 Implement the declaration mapping in `symbol_index/extractors/lean.rs` (design D3 and D4):
  - Ranges come from the outer `declaration` node.
  - Signatures drop attributes and keep modifiers.
  - Children: fields, constructors, `where` helpers and instance fields.
  - A missing name produces no symbol.

  Add tests in the `symbol_index/mod.rs` test module:

  | Scenario (`index-tool / …`) | Test |
  |---|---|
  | *Lean imports and opens are imports* | `lean_imports_and_opens_are_imports` |
  | *Lean definitions are functions* | `lean_definitions_are_functions` |
  | *Lean types and instances* (symbol kinds and children) | `lean_types_and_instances` |
  | *Lean axioms and constants* | `lean_axioms_and_constants` |
  | *Lean macro commands are macros* | `lean_macro_commands_are_macros` |
  | *Attributed declaration label and range* (start line on the attribute line; slicing `start_byte..end_byte` begins with `@[simp]`) | `attributed_declaration_label_and_range` |

  Verify with `cargo nextest run -p querymt-agent-index -E 'test(/lean_|attributed_declaration/)'`.
  - Verified: `cargo nextest run -p querymt-agent-index -E 'test(/lean_|attributed_declaration/)'` passed.
- [x] 3.3 Implement the scope stack (design D2):
  - `namespace`, `section`, `public_section` and `mutual` frames.
  - `end` matching, named and unnamed.
  - A never-closed namespace ends at EOF.
  - Dotted qualified names with `parent` set.

  Add tests in the `symbol_index/mod.rs` test module:

  | Scenario (`index-tool / …`) | Test |
  |---|---|
  | *Declarations nest under namespace* (symbol tree, `end` line, qualified names) | `declarations_nest_under_namespace` |
  | *Nested namespaces nest further* | `nested_namespaces_nest_further` |
  | *Unclosed namespace runs to end of file* | `unclosed_namespace_runs_to_end_of_file` |
  | *Sections and mutual blocks are transparent* | `sections_and_mutual_blocks_are_transparent` |
  | *Re-opened namespace yields separate entries* | `reopened_namespace_yields_separate_entries` |
  | *Dotted declaration name inside namespace* (`name == "Point.norm"`, `qualified_name == "Geometry.Point.norm"`, and `SymbolIndex::find_by_name("Geometry.Point.norm", None)` finds it) | `dotted_declaration_name_inside_namespace` |

  Verify with `cargo nextest run -p querymt-agent-index -E 'test(/namespace|sections_and_mutual|dotted_declaration/)'`.
  - Verified: namespace, section, mutual, dotted-name tests passed (plus an extra `stray_named_end_does_not_close_namespace`).
- [x] 3.4 Implement error recovery (design D6): recurse into `ERROR` nodes and ignore `MISSING` nodes. Covers *index-tool / Partial parse still yields declarations*, proved by `partial_parse_still_yields_declarations`, which uses custom-notation input followed by `def g` and asserts that `g` and its line range are present. Verify with `cargo nextest run -p querymt-agent-index -E 'test(/partial_parse_still_yields_declarations/)'`.
  - Verified: `partial_parse_still_yields_declarations` passed. Extra `lean_declaration_without_name_yields_no_symbol` covers D6's missing-name rule. A declaration swallowed inside an ill-formed `by` block is not recoverable (see design D3).

- [x] 3.5 Review fixes in the extractor (design D2/D4), each with a regression test in `symbol_index/mod.rs`:
  - `open`/`set_option`/`variable … in <command>`: the body is handled, not dropped (`lean_in_prefixes_do_not_hide_declarations`, `lean_in_prefix_inside_namespace_keeps_qualified_name`).
  - Dotted namespaces open one scope per component; `end` matches by trailing components (`dotted_namespace_closed_by_components`, `dotted_end_closes_nested_namespaces`).
  - `public section` pushes a section frame (`public_section_is_closed_by_end`, `unclosed_public_section_does_not_break_namespace`).
  - Comments are skipped in the label (`lean_comment_between_attribute_and_declaration_is_not_the_label`), anonymous instance names are whitespace-normalised (`lean_anonymous_instance_name_is_whitespace_normalised`), imports stay top level (`lean_open_inside_namespace_stays_top_level_import`), `_root_.` is stripped (`lean_root_prefix_ignores_enclosing_namespace`).
  - Verified: `cargo nextest run -p querymt-agent-index` (156 passed) and clippy clean.

## 4. Outline projection

- [x] 4.1 Change `symbols_to_sections` in `symbol_index/outline_projection.rs` to take `language: &str` and update its only caller, `outline_index::extractors::extract_outline`. Add the Lean-only branch (design D5):
  - Namespaces go to `namespaces` with their children nested. No `collect_namespace_members` call and no `split_interface_enum_sections`.
  - Top-level `theorem`/`lemma` functions, after Lean modifiers, go to a new `theorems` section pushed after `functions`.

  Add tests in `outline_index/tests.rs` through `extract_outline` or `index_file`:

  | Scenario (`index-tool / …`) | Test |
  |---|---|
  | *Top-level theorem and lemma* | `top_level_theorem_and_lemma` |
  | *Declarations nest under namespace* (no member in `types`/`functions`/`theorems`) | `lean_outline_declarations_nest_under_namespace` |
  | *Lean types and instances* (`Point`/`Color` in `types`, `Shape` in `traits`, the instance in `impls`) | `lean_outline_types_and_instances` |
  | *Lean examples are tests* (`include_tests: false` drops the example; the default lists it under `tests`) | `lean_examples_are_tests` |
  | *Theorems section is Lean-only* (Rust/PHP/C#/Kotlin/Swift sources containing `theorem`/`lemma` identifiers and, for PHP and C#, namespaces: no `theorems` section, and namespace members still hoisted as before) | `theorems_section_is_lean_only` |

  Extend `existing_languages_have_no_sections_or_markup` to also assert no `theorems` section; it covers *Existing language output unchanged*. Verify with `cargo nextest run -p querymt-agent-index -E 'test(/theorem|lean_outline|lean_examples|existing_languages_have_no_sections_or_markup/)'`.
  - Verified: `cargo nextest run -p querymt-agent-index -E 'test(/theorem|lean_outline|lean_examples|existing_languages_have_no_sections_or_markup/)'` passed (9 tests).
- [x] 4.2 Run the `querymt-agent-index` gate in order:
  1. `cargo check -p querymt-agent-index`
  2. `cargo t-index` (full package; existing-language extractor tests prove the Bash, PHP, Kotlin and Swift outline scenarios are unchanged)
  3. `cargo fmt -p querymt-agent-index -- --check`
  4. `cargo cl-index`

  Done when all four pass.
  - Results: `cargo check` ok; `cargo t-index` 146 passed, 1 skipped; `cargo fmt -- --check` ok; `cargo cl-index` ok.

## 5. Tool contract and downstream tools

- [x] 5.1 In `crates/agent-services/src/tools/builtins/index.rs`, add "Lean" after Swift in the `IndexTool` description and `lean` after `swift` in the `UnsupportedLanguage` error list.
  - Extend `test_index_description_mentions_new_languages` to assert `Lean`; covers *Tool description lists new languages*.
  - Extend `test_index_unsupported_extension` to assert `lean`; covers *Unsupported extension error is actionable*.
  - Add `test_index_lean_file`, which writes a `.lean` fixture with imports, a top-level `def`, a top-level `theorem` and a `structure`, and asserts `language: lean` and the `imports:`, `functions:`, `theorems:` and `types:` sections; covers *Lean file outline*.

  Verify with `cargo nextest run -p querymt-agent-services -E 'test(/test_index_/)'`.
  - Verified: `cargo nextest run -p querymt-agent-services -E 'test(/test_index_/)'` 11 passed.
- [x] 5.2 Add `get_symbol` and `replace_symbol` tool tests:
  - In `get_symbol.rs`, add `lookup_by_qualified_name`, which requests `Geometry.dist` from a `.lean` file and asserts the `def dist` body is returned; covers *Lookup by qualified name*.
  - In `replace_symbol.rs`, add `replace_covers_attributes`, which replaces `foo` declared as `@[simp] theorem foo …` and asserts that the file content afterwards has no remaining `@[simp]` from the old declaration; covers *Replace covers attributes*.

  Verify with `cargo nextest run -p querymt-agent-services -E 'test(/lookup_by_qualified_name|replace_covers_attributes/)'`.
  - Verified: `cargo nextest run -p querymt-agent-services -E 'test(/lookup_by_qualified_name|replace_covers_attributes/)'` 2 passed.
- [x] 5.3 Add `lean_files_extract_symbols` to `crates/agent-knowledge/src/code.rs`. It asserts that `extract_code("Foo/Bar.lean", …)` yields `CodeStatus::Extracted`, `language == Some("lean")` and the declared symbol (proposal Impact). Verify with `cargo nextest run -p querymt-agent-knowledge -E 'test(/lean_files_extract_symbols/)'`.
  - Verified: `cargo nextest run -p querymt-agent-knowledge -E 'test(/lean_files_extract_symbols/)'` 1 passed.
- [x] 5.4 Add an "Unreleased → Added" CHANGELOG.md entry: Lean 4 support in `index`, `get_symbol`, `get_function`, `find_symbol_references` and `replace_symbol`, the new `theorems` section, and the `symbols_to_sections` signature change. Done when the entry is present.
  - Done: entry added under Unreleased → Added in CHANGELOG.md.
- [x] 5.5 Run the `querymt-agent-services` and `querymt-agent-knowledge` gates in order: `cargo check`, then `cargo t-services` and `cargo t-knowledge`, then `cargo fmt -p querymt-agent-services -p querymt-agent-knowledge -- --check`, then `cargo cl-services` and `cargo cl-knowledge`. Done when all pass.
  - Results: `cargo check` ok; `cargo t-services` 1034 passed, 4 skipped; `cargo t-knowledge` 446 passed; `cargo fmt -- --check` ok; `cargo cl-services` and `cargo cl-knowledge` ok.

## 6. Integration checks

- [x] 6.1 List the direct dependents with `cargo tree -i querymt-agent-index --workspace --depth 1`. For each one not covered in group 5, run `cargo check -p <package>`. Build the facade with `QMT_UI_DIST=<fixture> cargo t-agent --no-run`, using the minimal fixture from `.github/actions/rust-setup/action.yml`. Done when all compile.
  - Results: direct dependents are `querymt-agent`, `querymt-agent-knowledge`, `querymt-agent-runtime`, `querymt-agent-services`. `cargo check -p querymt-agent-runtime` compiled (15 existing dead-code warnings, unrelated; default features). `QMT_UI_DIST=/tmp/qmt-dashboard-ui cargo t-agent --no-run` built with the minimal fixture from `rust-setup/action.yml`. The zbus/systemd features only take effect on Linux and were not verified on macOS.
- [x] 6.2 If `nix` is available, run the `querymt-agent` package build from `flake.nix` to confirm the git dependency fetches under `allowBuiltinFetchGit`. Otherwise record that this was not verified locally and is left to the `nix` CI workflow.
  - Not verified locally: `nix` is not installed on this machine. The git dependency fetch under `allowBuiltinFetchGit` is left to the `nix` CI workflow.
- [x] 6.3 Run `openspec validate add-lean4-index-support --strict`. Done when it passes.
  - Verified: `openspec validate add-lean4-index-support --strict` reported the change is valid.

## Workflow follow-up

- Ask upstream (Julian/tree-sitter-lean) about publishing to crates.io, and switch from the git pin when a release exists.
- Archive the change after review, then confirm that `openspec/specs/index-tool/spec.md` contains the Lean requirements.
