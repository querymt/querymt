# Tasks

Package: `qmt-llama-cpp` only. Features for every command: `--no-default-features --features native`. Abbreviations used below:

- `NT` = `cargo nextest run -p qmt-llama-cpp --no-default-features --features native`
- `LIVE` = `QMT_RUN_LIVE_LLAMA_LORA=1 QMT_LLAMA_VOCAB_DOWNLOAD=1` prefixed to `NT --run-ignored only`

Rust rules referenced are from the rust-skills set (design.md lists where each applies).

## 1. Config types and parsing

- [x] 1.1 Add `thiserror = { workspace = true }` to `[dependencies]` and `toml = { workspace = true }` to `[dev-dependencies]` of `crates/providers/llama-cpp/Cargo.toml` (crate is excluded from hakari, so no workspace-hack update). Verify: `cargo check -p qmt-llama-cpp --no-default-features --features native` succeeds.
- [x] 1.2 In `config.rs`, add `LoraScale` (`#[serde(try_from = "Option<f32>", into = "f32")]`, `const fn new/get/is_disabled`, `Default` = 1.0, `TryFrom<f32>`, `TryFrom<Option<f32>>`, `From<LoraScale> for f32`, hand-written `JsonSchema`), `LoraScaleError` (`thiserror`, lowercase message, no trailing punctuation), and `#[non_exhaustive]` `LoraAdapterConfig` (`deny_unknown_fields`, `#[serde(default)] scale`, `#[must_use] new`/`with_scale`) per design decision 1. Add `lora: Vec<LoraAdapterConfig>` to `LlamaCppConfig` with `#[serde(default, skip_serializing_if = "Vec::is_empty")]` and a `///` doc comment. Document every public item with `# Errors` where fallible and a doctest on `LoraScale::new` using `?`. Re-export `LoraAdapterConfig`, `LoraScale`, `LoraScaleError` from `lib.rs`. Verify: `cargo test -p qmt-llama-cpp --no-default-features --features native --doc` passes.
- [x] 1.3 Add `lora: Vec::new()` to every in-repo `LlamaCppConfig` struct literal (superseded: `LlamaCppConfig` derives `Default` and has `new(model)`, and the in-repo literals now use `..Default::default()`) (`tests/integration_test.rs`, `tests/multimodal_test.rs`, `examples/vision_chat.rs`, and test helpers in `messages.rs`, `multimodal.rs`, `placement.rs`, `provider.rs`). Verify: `cargo check -p qmt-llama-cpp --no-default-features --features native --all-targets` succeeds.
- [x] 1.4 Parse tests in `config.rs` `mod tests`, through `serde_json::from_str::<LlamaCppConfig>` (the factory path) and `toml::from_str` (for `nan`/`inf` literals):
  - llama-cpp-lora-adapters / Lora omitted keeps the base model → `lora_omitted_keeps_the_base_model` (no key parses to an empty list; serializing an empty list omits the key).
  - llama-cpp-lora-adapters / Adapter scale defaults to one → `adapter_scale_defaults_to_one` (`LoraScale::get() == 1.0` exactly; an explicit `scale = -0.5` is accepted).
  - llama-cpp-lora-adapters / Unknown adapter key rejected → `unknown_adapter_key_rejected` (error text contains `weight`).
  - llama-cpp-lora-adapters / Non-finite adapter scale rejected → `non_finite_adapter_scale_rejected` (TOML `nan`, `inf`, `-inf`, and JSON `null`, each error contains "must be a finite number"; `LoraScale::new(f32::NAN)` is `Err`).
  - Also assert the `config_schema` JSON contains a `lora` property with `path` and `scale`, in `test_config_schema_generation` of `tests/integration_test.rs`.

  Verify: `NT -E 'test(/lora_omitted_keeps_the_base_model|adapter_scale_defaults_to_one|unknown_adapter_key_rejected|non_finite_adapter_scale_rejected|test_config_schema_generation/)'` passes.
- [x] 1.5 Group checks: `cargo fmt -p qmt-llama-cpp -- --check` and `cargo clippy -p qmt-llama-cpp --no-default-features --features native --all-targets -- -D warnings` pass with no new `#[allow]`. Result: `cargo fmt -p qmt-llama-cpp -- --check` passes and `cargo clippy -p qmt-llama-cpp --no-default-features --features native --all-targets -- -D warnings` passes with no new `#[allow]`. These pre-existing clippy fixes should land as their own commit, separate from the LoRA change. The 32 lib / 37 lib-test errors that pristine HEAD reports were cleared by behavior-preserving fixes (let-chains, `checked_div`, `is_none_or`/`is_some_and`, iterator loops, a derived `Default`, a `ParsedToolResponse` alias, `MediaInputs` bundling `mm_ctx`+`bitmaps`, `&Path`, moving `decoding_tests` last).

## 2. Adapter resolution and validation

- [x] 2.1 Create `src/lora.rs` (with `//!` module docs) holding `LoraError` and `impl From<LoraError> for LLMError` (decision 2): `Apply` maps to `ProviderError`, all other variants map to `InvalidRequest`, and the `source()` chain is joined into the message. Register `mod lora;` in `lib.rs`. Verify: unit test `lora_error_message_includes_source_chain` passes with `NT -E 'test(/lora_error_message_includes_source_chain/)'`.
- [x] 2.2 Implement `locate_lora_adapters` (offline: count check, `parse_model_ref` for all entries, local existence) and `LocatedAdapters::download` (Hugging Face download with `download_hf_file_sync`; non-UTF-8 or NUL paths give `InvalidRef`; download failures give the retryable `Download`). `LlamaCppProvider::resolve_lora_paths` also parses the `model`/`speculative.model` refs between the two steps. Call it at the start of `LlamaCppProvider::new` and `new_with_cache`, before `resolve_model_path(&cfg.model)`. Unit tests in `lora.rs`:
  - llama-cpp-lora-adapters / Missing adapter file rejected → `missing_adapter_file_rejected` (error names `lora[0].path` and the path).
  - llama-cpp-lora-adapters / Hugging Face adapter without selector rejected → `hugging_face_adapter_without_selector_rejected` (`owner/repo` gives an error naming `lora[0].path` and asking for a selector; no network used).
  - llama-cpp-lora-adapters / More than one adapter rejected → `more_than_one_adapter_rejected` (error names `lora` and says only one adapter is supported; the count check runs before any path check, shown by two nonexistent paths).
  - Plus `more_than_one_adapter_rejected_before_model_load`: `LlamaCppProvider::new` with a nonexistent `model` and two adapters returns the `lora` error, not "Model path does not exist", proving validation runs before model resolution.

  Verify: `NT -E 'test(/missing_adapter_file_rejected|hugging_face_adapter_without_selector_rejected|more_than_one_adapter_rejected/)'` passes.
- [x] 2.3 Group checks: fmt and clippy as in 1.5. Result: fmt and clippy `-D warnings` pass (see 1.5).

## 3. Loading, ownership and cache key

- [x] 3.1 Add `LoadedLora`, `LoraSet` (field order: `adapters: Box<[LoadedLora]>` then `model: Option<Arc<LlamaModel>>`; `Default` empty; single constructor `LoraSet::load(model, paths)`), the `const` Send + Sync assertion, and the pure helper `active_scales(&[LoraAdapterConfig])` that yields `(index, scale)` and skips `LoraScale::is_disabled` (decision 3). Lock with `lock().unwrap_or_else(PoisonError::into_inner)`; no `unwrap()`. Verify: unit test `active_scales_skips_disabled_adapters` (scale 0 and -0.0 are skipped, 0.5 kept) passes with `NT -E 'test(/active_scales_skips_disabled_adapters/)'`.
- [x] 3.2 Change `load_models` to take `lora_paths: &[PathBuf]`, load the target model as `mut`, call `lora_adapter_init` per path (map `NullResult` and other init errors to `LoraError::Incompatible`), then wrap in `Arc`, and return a `LoadedModels { model, mtp_model, loras: Arc<LoraSet> }` struct. Never init adapters on the MTP sidecar. Log one `log::info!` per loaded adapter. Add `loras: Arc<LoraSet>` to `CachedModel` and `LlamaCppProvider`, and populate it on both the cache-hit and load paths. Verify: `cargo check -p qmt-llama-cpp --no-default-features --features native --all-targets` succeeds.
- [x] 3.3 Add `lora_paths: Box<[String]>` to `ModelCacheKey` (resolved adapter paths in config order; scales excluded). Unit tests in `provider.rs` `mod tests`:
  - llama-cpp-lora-adapters / Changed adapter list reloads the model → `changed_adapter_list_reloads_the_model` (keys built from `[a]`, `[b]` and `[]` are pairwise unequal).
  - llama-cpp-lora-adapters / Changed scale reuses the loaded model → `changed_scale_reuses_the_loaded_model` (the key built from `[a @ 1.0]` equals the key from `[a @ 0.5]`). Extract the key construction from `new_with_cache` into `fn cache_key(cfg, model_path, lora_paths, placement)` so the test goes through the production code.

  Verify: `NT -E 'test(/changed_adapter_list_reloads_the_model|changed_scale_reuses_the_loaded_model/)'` passes.
- [x] 3.4 Group checks: fmt and clippy as in 1.5. Result: fmt and clippy `-D warnings` pass (see 1.5); `NT` 185 passed, 11 skipped.

## 4. Applying adapters to every generation context

- [x] 4.1 Add `Models<'a> { target, draft, loras }` (`Clone + Copy`), use `MediaInputs` (`mm_ctx` + `bitmaps`) in every generation entry point, and change these functions to take `models: Models<'_>` instead of `model` + `mtp_model` / `draft_model`:
  - `generate`
  - `generate_streaming_with_thinking`
  - `generate_with_tools`
  - `generate_streaming_with_tools`
  - `prefill_for_tool_generation`
  - `run_mtp`

  Thread `Arc<LoraSet>` into the streaming `thread::spawn` closures in `provider.rs` by cloning the `Arc` before the move. Do not add new `#[allow(clippy::too_many_arguments)]`. Verify: `cargo check -p qmt-llama-cpp --no-default-features --features native --all-targets` succeeds and existing tests pass with `NT`.
- [x] 4.2 Implement `LoraSet::apply` and `lora::new_target_context` (decision 5). Switch the five target-context sites to the helper: `generate` (both sampler branches), `generate_streaming_with_thinking`, both branches of `prefill_for_tool_generation`, and the target context in `run_mtp`. Keep each site's memory-estimate error message. Leave the `run_mtp` draft context on `draft_model.new_context`, with a comment that it intentionally gets no adapter.
  - llama-cpp-lora-adapters / Adapter applies to tool-call, multimodal and speculative generation → source-guard unit test `adapter_applies_to_tool_call_multimodal_and_speculative_generation` in `lora.rs`. It `include_str!`s every `src` file that creates contexts and asserts:
    - `.new_context*` calls appear only in `lora.rs`, plus exactly one draft call in `mtp.rs`, and no unlisted `src` file creates a context;
    - `new_target_context(` is used in `generation.rs` (2), `tools/prefill.rs` (2) and `mtp.rs` (1).

    Multimodal and MTP have no small public fixture with a matching LoRA, so the guard is how those paths are verified; the tool-call path also gets a live check in 5.4.

  Verify: `NT -E 'test(/adapter_applies_to_tool_call_multimodal_and_speculative_generation/)'` passes.
- [x] 4.3 Group checks: full `NT` suite, fmt and clippy as in 1.5. Record the pass/skip counts here. Result: full `NT` 185 passed, 11 skipped; doctest 1 passed; fmt passes; clippy `-D warnings` passes (see 1.5).

## 5. Live checks with a real adapter

- [x] 5.1 Add pinned fixtures to `test_vocab.rs`: `LORA_BASE_MODEL` (`ggml-org-dev/stories15M_MOE`, rev `b79d85fd44cb281d525c0f75e79b522b9fdf714b`, `stories15M_MOE-F16.gguf`) and `LORA_ADAPTER` (same repo/rev, `moe_shakespeare15M.gguf`), fetched with `ensure_full_model`. Add a live helper in `lora.rs` tests that skips with an `eprintln!("SKIPPED: …")` unless `QMT_RUN_LIVE_LLAMA_LORA` is set (mirroring `QMT_RUN_LIVE_LLAMA_MTP`), and builds configs with `temperature = 0`, `seed = 42`, `max_tokens = 32`, `n_ctx = 512`, `n_gpu_layers = 0`. Verify: `NT -E 'test(/lora_live/)'` lists the tests as ignored, and `LIVE -E 'test(/lora_live_fixtures_load/)'` passes.
- [x] 5.2 Completion tests on prompt `"Look in thy glass"` (`#[ignore = "live: set QMT_RUN_LIVE_LLAMA_LORA=1 and use --run-ignored"]`):
  - llama-cpp-lora-adapters / Adapter changes generated text → `adapter_changes_generated_text_lora_live` (output with scale 1.0 differs from no-`lora` output and matches `(eye|love|glass|sun)`, the expectation from llama.cpp's `test_lora.py`).
  - llama-cpp-lora-adapters / Zero scale matches the base model → `zero_scale_matches_the_base_model_lora_live` (scale 0 output equals no-`lora` output byte-for-byte).

  Verify: `LIVE -E 'test(/adapter_changes_generated_text_lora_live|zero_scale_matches_the_base_model_lora_live/)'` passes.
- [x] 5.3 llama-cpp-lora-adapters / Adapter applies to streaming output → `adapter_applies_to_streaming_output_lora_live`. Use `chat_stream` with a minimal `chat_template` (the fixture has none). Collect text chunks for scale 1.0, scale 0 and no `lora`. Assert 1.0 differs from 0, and 0 equals no-`lora`. Verify: `LIVE -E 'test(/adapter_applies_to_streaming_output_lora_live/)'` passes.
- [x] 5.4 llama-cpp-lora-adapters / Adapter applies to tool-call, multimodal and speculative generation, tool-call part → `adapter_applies_to_tool_call_generation_lora_live`. Call `prefill_for_tool_generation` with `Models` for scale 1.0 and scale 0 on the same prompt. Assert the last-position logits differ, and that scale 0 logits equal those from an empty `LoraSet`. Verify: `LIVE -E 'test(/adapter_applies_to_tool_call_generation_lora_live/)'` passes.
- [x] 5.5 Incompatibility and reuse through the factory (`create_factory().from_config(json)`): Result: `LIVE -E "test(/_lora_live/)"` 8 passed (including 5.1-5.4 tests), 187 skipped; without the env vars they skip with a SKIPPED note.
  - llama-cpp-lora-adapters / Incompatible adapter rejected → `incompatible_adapter_rejected_lora_live`. Use the base model file as its own adapter. Assert the error names `lora[0].path` and the path. Then configure the same factory with the valid adapter and assert it succeeds, so the process survived.
  - llama-cpp-lora-adapters / Changed scale reuses the loaded model → `changed_scale_reuses_the_loaded_model_lora_live`. Configure two providers that differ only in scale. Assert `Arc::ptr_eq` on their `loras` and that their completions differ. Downcast via a `#[cfg(test)]` accessor; if downcasting `Box<dyn LLMProvider>` is not possible, build both through `LlamaCppProvider::new_with_cache` with a shared cache.
  - llama-cpp-lora-adapters / Changed adapter list reloads the model → `changed_adapter_list_reloads_the_model_lora_live`. Configure with the adapter, then without one. Assert the second provider's model `Arc` is not `ptr_eq` with the first and its `LoraSet` is empty.

  Verify: `LIVE -E 'test(/_lora_live/)'` passes. Record the counts here.

## 6. Documentation and changelog

- [x] 6.1 In `docs/docs/core/available-providers.md`, add a `lora` row to the key-options table and a "LoRA adapters" section. It covers:
  - a TOML example (`lora = [{ path = "/models/style-lora.gguf", scale = 0.8 }]`)
  - `scale` semantics (finite, default 1, 0 disables, negative allowed)
  - Hugging Face refs
  - the one-adapter limit
  - converting PEFT adapters with llama.cpp's `convert_lora_to_gguf.py`
  - reload vs. reuse (paths reload, scales don't)
  - aLoRA treated as plain LoRA
  - adapters not applied to the MTP draft (lower acceptance)
  - adapter memory not in the advisory

  Verify: unit test `documented_lora_example_parses` in `config.rs` parses the documented TOML snippet (kept as a `const` string that matches the docs) into `LlamaCppConfig`, and `NT -E 'test(/documented_lora_example_parses/)'` passes.
- [x] 6.2 Add CHANGELOG.md `## Unreleased` entries: under `### Added`, the `lora` provider option. Under `### Breaking`, "`qmt_llama_cpp::LlamaCppConfig` has a new public field `lora: Vec<LoraAdapterConfig>`; struct literals must add `lora: Vec::new()`", plus the new exports `LoraAdapterConfig`, `LoraScale`, `LoraScaleError`. Verify: entries present under the right headings (`grep -n "lora" CHANGELOG.md`).

## 7. Integration

- [x] 7.1 Confirm `cargo tree -i qmt-llama-cpp --workspace --depth 1 -e normal,dev` still lists no workspace dependents, so no downstream package needs testing. Then run the full package suite `NT`, `cargo test -p qmt-llama-cpp --no-default-features --features native --doc`, `cargo fmt -p qmt-llama-cpp -- --check` and `cargo clippy -p qmt-llama-cpp --no-default-features --features native --all-targets -- -D warnings`. Verify: all pass; record the counts here. Result: `cargo tree -i` lists only qmt-llama-cpp (no workspace dependents); `NT` 185 passed, 11 skipped; doctest 1 passed; fmt passes; clippy `-D warnings` passes; `LIVE -E 'test(/lora_live/)'` 8 passed, 188 skipped.
- [x] 7.2 `openspec validate add-llama-cpp-lora-adapters --strict` passes.

## Follow-ups (not part of this change)

- Upstream multi-adapter API (formerly task 7.1): open a PR to `utilityai/llama-cpp-rs` adding `LlamaContext::set_lora_adapters(&mut self, &[(&LlamaLoraAdapter, f32)])` (a slice passed to `llama_set_adapters_lora`) and keeping `lora_adapter_set` as a wrapper over it. Done when the PR exists and its CI passes. Needs an external fork and upstream CI.
- Lift the one-adapter limit in this provider (a separate change, after the upstream API is available). The config is already a list, so this is not breaking.

## Workflow follow-up

- Archive the change after review with `/openspec-archive-change`, once every scenario above has a passing test or its documented verification.
