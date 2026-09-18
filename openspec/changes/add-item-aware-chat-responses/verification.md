# Verification Record

Task 6.4 — focused core/provider/agent/binding tests and the applicable
workspace feature/build matrix.

## Successful commands

### Core

- `cargo test -p querymt --lib` → 178 passed, 0 failed
- `cargo check -p querymt --no-default-features` → 0 errors
- `cargo check -p querymt-extism-macros` → 0 errors

### Providers

- `cargo test -p qmt-openai --features native --lib` → 83 passed, 0 failed
- `cargo test -p qmt-codex --features native --lib` → 54 passed, 0 failed
- `cargo test -p querymt-remote --lib` → 81 passed, 0 failed
- Feature matrix, `native` and `extism` for `qmt-openai`, `qmt-codex`, `qmt-xai`
  → 0 errors
- `cargo check -p <provider> --features native` for `qmt-anthropic`, `qmt-groq`,
  `qmt-deepseek`, `qmt-mistral`, `qmt-openrouter`, `qmt-moonshot`, `qmt-alibaba`,
  `qmt-zai`, `qmt-kimi-code` → 0 errors
- `cargo test -p <provider> --features native --lib` → all passed
  (anthropic 30, groq 4, deepseek 1, openrouter 10, kimi-code 21; others 0 tests)

### Agent

- `cargo test -p querymt-agent --lib` → 2107 passed, 0 failed, 3 ignored

## Environment-limited checks

- `cargo check -p querymt-py`: build-script failure from `pyo3-ffi` because the
  system interpreter is Python 3.9.6 while the crate requires `abi3-py310`
  (≥ 3.10). Environmental; unrelated to this change.
- `cargo check -p querymt-mobile-ffi`: pre-existing compile errors unrelated to
  this change. The working tree does not modify `querymt-mobile-ffi`
  (`git diff --name-only HEAD` excludes it).

## Pre-existing failures (not caused by this change)

- `qmt-xai` `parse_list_models_*` (3 tests): stale expectations versus
  `XAI_ADDITIONAL_LIST_MODELS`, which already included `grok-4.5` at `HEAD`
  (commit `4a56b10e`). Diff confirms this change does not touch
  `parse_list_models`, `XAI_ADDITIONAL_LIST_MODELS`, or
  `is_supported_xai_list_model`.

## Legacy Chat Completions regressions

- `qmt-openai` Chat Completions snapshot tests assert the default mode keeps the
  `/chat/completions` endpoint, the `messages` body, and omitted strictness
  semantics; all pass.
