# Tasks

Package for every command: `qmt-llama-cpp`. Features: `--no-default-features --features native`, abbreviated as `$F`.

## 1. Baseline (commit the existing branch work)

- [x] 1.1 Commit the uncommitted `perf/llamacpp-grammar` work: collapsed `ExclusionDfa`, `ConstrainedSampler`, `tools/llg.rs`, `GrammarBackend`. Verify with `cargo check -p qmt-llama-cpp $F` and `cargo nextest run -p qmt-llama-cpp $F` (all pass, `grammar_cost_benchmark` ignored).
  - Result: `cargo check -p qmt-llama-cpp --no-default-features --features native --all-targets` passed; `cargo nextest run -p qmt-llama-cpp --no-default-features --features native`: 124 passed, 1 skipped (`grammar_cost_benchmark`). Committed as `dabbe9ee` (only `crates/providers/llama-cpp`).
- [x] 1.2 Rename the existing tests to their scenario names and keep them passing. Covers:
  - local-tool-call-grammars / Value accepts less-than and markup (`value_accepts_less_than_and_markup`, from `grammar_does_not_mask_lt_inside_string_values` plus `qwen_function_accepts_lt_in_values`)
  - local-tool-call-grammars / Value cannot contain its terminator (`value_cannot_contain_its_terminator`, from `qwen_function_rejects_terminator_inside_value` plus `rendered_rules_match_substring_exclusion_exhaustively`)
  - local-tool-call-grammars / GBNF text values stay narrow (`gbnf_text_values_stay_narrow`, from `collapsed_rules_stay_narrow`)
  - local-tool-call-grammars / Rejected favourite is resampled from allowed tokens (`rejected_favourite_is_resampled_from_allowed_tokens`, from `rejection_sampling_matches_grammar_first`)
  - local-tool-call-grammars / Multi-token trigger activates the grammar (`multi_token_trigger_activates_the_grammar`, from `llguidance_backend_matches_gbnf_on_lt_and_close` on qwen35, where `<tool_call>` is 4 tokens)

  Verify with `cargo nextest run -p qmt-llama-cpp $F -E 'test(/value_|gbnf_text|rejected_favourite|multi_token_trigger/)'`.
  - Result: `cargo nextest run -p qmt-llama-cpp --no-default-features --features native -E 'test(/value_|gbnf_text|rejected_favourite|multi_token_trigger/)'`: 8 passed, 117 skipped (includes the existing `chat_format::tests::qwen_function_value_may_contain_markers_and_html`).

- [x] 1.3 Generalise `grammar_tests::vocab_path` into `vocab_model(Vocab)` for qwen35, gemma-4, LFM2 and FunctionGemma (sources and pins in design D7). llama.cpp vocabs are fetched whole; Hugging Face models are fetched as a `Range` prefix (8 MiB, doubled up to 64 MiB until the KV section fits), cut at the end of the GGUF key/value section and written with tensor count 0. Download only if missing, honour `QMT_LLAMA_VOCAB_DIR`, skip with a message when offline, fail loudly if the KV section does not fit in 64 MiB. Downloads are opt-in (`QMT_LLAMA_VOCAB_DOWNLOAD=1`); without it only the cache is used and tests skip. Covers:
  - local-tool-call-grammars / Hugging Face tokenizers are fetched at the pinned revision (`hugging_face_tokenizers_are_fetched_at_the_pinned_revision`, `#[ignore]`, run with `QMT_LLAMA_VOCAB_DOWNLOAD=1 --run-ignored only`: empty temp `QMT_LLAMA_VOCAB_DIR`, fetch, assert the stored file is under 64 MiB and a second call does no request)
  - local-tool-call-grammars / Real tokenizers report the expected vocabulary (`real_tokenizers_report_the_expected_vocabulary`: `n_vocab` 151,936 / 262,144 / 65,536 / 262,144 and delimiter tokenization)
  - local-tool-call-grammars / Offline run skips real-tokenizer checks (`offline_run_skips_real_tokenizer_checks`: unreachable base URL through a test hook, assert skip result naming the model)
  - local-tool-call-grammars / Changed pinned model file fails loudly (`changed_pinned_model_file_fails_loudly`: a local fixture GGUF whose KV section exceeds the limit, served through the same test hook, assert the error names repo, revision and file)

  Verify with `cargo nextest run -p qmt-llama-cpp $F -E 'test(/tokenizers_|offline_run_skips|changed_pinned/)'`.
  - Result: helper in `src/test_vocab.rs` (`Vocab`, `vocab_model`, GGUF key/value scanner, injectable fetch/hosts/limits). `cargo nextest run -p qmt-llama-cpp --no-default-features --features native -E 'test(/tokenizers_|offline_run_skips|changed_pinned|prefix_fetch|kv_section/)'`: 6 passed (real downloads of gemma-4, LFM2, FunctionGemma succeeded; n_vocab and delimiter checks pass). Full package suite after group 1: 130 passed, 1 skipped; `cargo fmt --check` clean.
  - Note for 8.1: `cargo clippy -p qmt-llama-cpp ... -D warnings` already fails with 41 errors on the commit before this change (files such as `generation.rs`, `provider.rs`, `sampler.rs:54`). None are in new code; to be handled in 8.1.

## 2. Rejection sampling and decoding paths

- [x] 2.1 Add a test-only counter of whole-vocabulary masks to `ConstrainedSampler`, and test local-tool-call-grammars / Allowed favourite is kept without full mask (`allowed_favourite_is_kept_without_full_mask`). Verify that it returns the favourite and the counter stays 0.
  - Result: `ConstrainedSampler::full_masks()` (cfg(test)); `allowed_favourite_is_kept_without_full_mask` passes for GBNF and llguidance (counter 0 inside a value, exactly 1 after a rejected favourite). Run: `cargo nextest run -p qmt-llama-cpp --no-default-features --features native -E 'test(/allowed_favourite|forced_end_of_reasoning|rejected_favourite/)'`: 3 passed.
- [x] 2.2 Test local-tool-call-grammars / Forced end of reasoning keeps grammar state (`forced_end_of_reasoning_keeps_grammar_state`): force `</think>\n` through `force_text`'s `AcceptToken` path on a `ConstrainedSampler`, then feed `<tool_call>` and check that the grammar is active (a forbidden token is rejected). Uses the qwen35 vocab and skips when offline.
  - Result: `forced_end_of_reasoning_keeps_grammar_state` passes for both backends (same run as 2.1). It drives `AcceptToken::accept_token`, the path `force_text` uses.
- [x] 2.3 Test local-tool-call-grammars / Speculative decoding respects the tool grammar (`speculative_decoding_respects_the_tool_grammar`): load `unsloth/Qwen3.5-0.8B-MTP-GGUF` `Qwen3.5-0.8B-Q4_0.gguf` at the pinned revision (design D7, cached in the vocab dir), configure `speculative = { type = "mtp" }` with one tool, generate through the provider's tool path with `grammar_backend` set to each engine, and assert every delivered token sequence after the trigger is accepted by the grammar and the parsed call validates against the tool schema. `#[ignore]`, gated by `QMT_RUN_LIVE_LLAMA_MTP=1`. Also covers local-tool-call-grammars / MTP is checked on a real MTP model (the test asserts the loaded file's `nextn_predict_layers` metadata is non-zero and returns early without the variable). Verify with `QMT_RUN_LIVE_LLAMA_MTP=1 cargo nextest run -p qmt-llama-cpp $F --run-ignored only -E 'test(/speculative_decoding_respects/)'` and record the result here.
  - Result: `QMT_RUN_LIVE_LLAMA_MTP=1 QMT_LLAMA_VOCAB_DOWNLOAD=1 cargo nextest run -p qmt-llama-cpp --no-default-features --features native --run-ignored only --no-capture -E 'test(/speculative_decoding_respects/)'`: 1 passed (9.96 s, Apple M4 Pro/Metal; model downloaded once, 521 MB, then cached). Asserts `qwen35.nextn_predict_layers > 0`, replays the delivered text through a fresh grammar for both engines, and validates the parsed `write_file` call. Without `QMT_RUN_LIVE_LLAMA_MTP` the test returns early (checked).

## 3. Engine setting and fallbacks

- [x] 3.1 Flip `GrammarBackend::default()` to `Llguidance` and update the `LlamaCppConfig::grammar_backend` doc comment. Covers:
  - local-tool-call-grammars / Grammar backend defaults to llguidance (`grammar_backend_defaults_to_llguidance`: config without the key → sampler uses llguidance)
  - local-tool-call-grammars / Grammar backend gbnf selects GBNF (`grammar_backend_gbnf_selects_gbnf`)
  - local-tool-call-grammars / Grammar backend rejects unknown value (`grammar_backend_rejects_unknown_value`: serde error mentions `grammar_backend`)

  Verify with `cargo nextest run -p qmt-llama-cpp $F -E 'test(/grammar_backend_/)'`.
  - Result: 3 passed (`config::tests::grammar_backend_rejects_unknown_value`, `grammar_backend_defaults_to_llguidance`, `grammar_backend_gbnf_selects_gbnf`).
- [x] 3.2 Implement D5: `Constraint::Llg` keeps the GBNF `ToolGrammar` and the accepted tokens, and on a matcher error swaps to GBNF and replays them. Log once per request. Test local-tool-call-grammars / Matcher error switches to GBNF (`matcher_error_switches_to_gbnf`): inject an error through a test hook, then check that a token GBNF forbids is still rejected afterwards.
  - Result: `Constraint::Llg(LlgState)` keeps the GBNF grammar and accepted tokens; `Constraint::recover` swaps to GBNF once (logged via `llg::note_fallback`). `matcher_error_switches_to_gbnf` passes.
- [x] 3.3 Test local-tool-call-grammars / Llguidance build failure uses GBNF (`llguidance_build_failure_uses_gbnf`): force `LlgToolGrammar::build` to error through a test hook, then check that the sampler is GBNF and the request path succeeds.
  - Result: `llguidance_build_failure_uses_gbnf` passes (hook `llg::test_hooks::FORCE_BUILD_ERROR`, one fallback counted).
- [x] 3.4 Implement the D4 tiers: `Schema` → `Generic` → GBNF, with `JsonCompileOptions { coerce_one_of: true, lenient: true }`. Test local-tool-call-grammars / Uncompilable schema falls back to generic grammar (`uncompilable_schema_falls_back_to_generic_grammar`) with a schema the compiler rejects (for example an unresolvable `$ref`). Check that the generic tier is active and one fallback is logged (`testing_logger` or a counter).
  - Result: `Tier::{Schema,Generic}`; `LlgToolGrammar::build` tries Schema, then Generic (one `note_fallback`), then returns Err so the sampler uses GBNF. Array/object parameter values use `%json <schema>` with `x-guidance {coerce_one_of, lenient}` (the `%json` syntax has no option arguments). `uncompilable_schema_falls_back_to_generic_grammar` passes (unresolvable `$ref`; control schema stays on Schema tier with 0 fallbacks). Group 3 suite: `cargo nextest run -p qmt-llama-cpp --no-default-features --features native`: 137 passed, 3 skipped.

## 4. Lark ports

- [x] 4.1 Add the `tier` parameter to `qwen_function_lark` and `ministral_lark`. Ministral's schema tier uses per-tool `%json` argument schemas.
  - Result: both take `tier: Tier`; `ministral_grammar_compiles_and_matches` now checks required/type/enum rejection on the schema tier and acceptance of `{}` on the generic tier.
- [x] 4.2 Qwen3 JSON port (`qwen3_lark`, both tiers): `{"name": NAME, "arguments": ARGS}`, then `</tool_call>`, repeatable.
- [x] 4.3 LFM2 port (`lfm2_lark`, both tiers): `[name(k=v, …)]<|tool_call_end|>`. Schema tier uses typed keyword values in schema order.
- [x] 4.4 FunctionGemma port (`function_gemma_lark`, both tiers): `call:NAME{k:<escape>TEXT<escape>,…}<end_function_call>`, with the D3 special-token terminator encoding.
- [x] 4.5 Gemma 4 port (`gemma4_lark`, both tiers): `call:NAME{…}<tool_call|>`, recursive objects and arrays, `<|"|>` strings through D3, typed top-level parameters.
- [x] 4.6 Wire every format into `LlgToolGrammar::build`. Test local-tool-call-grammars / Every detected format compiles under llguidance (`every_detected_format_compiles_under_llguidance`): all six `ToolFormat` grammars build an `Llg` constraint on the format's real vocabulary (helper from 1.3).
- [x] 4.7 Byte-level language tests per format. Use `ApproximateTokEnv` for ordinary delimiters and a synthetic `TokTrie` with `0xFF` special delimiters for Gemma 4, FunctionGemma and LFM2 (fast, no download; the real-vocab checks are in group 5). Covers:
  - local-tool-call-grammars / Schema rejects wrong value type (`schema_rejects_wrong_value_type`, all six formats)
  - local-tool-call-grammars / Schema rejects value outside enum (`schema_rejects_value_outside_enum`)
  - local-tool-call-grammars / Schema requires required parameters (`schema_requires_required_parameters`)
  - local-tool-call-grammars / Schema rejects undeclared parameter when additional properties are forbidden (`schema_rejects_undeclared_parameter_when_additional_properties_are_forbidden`)
  - local-tool-call-grammars / Schema rejects unknown tool name (`schema_rejects_unknown_tool_name`)
  - local-tool-call-grammars / Value accepts less-than and markup for FunctionGemma and Gemma 4 (extend `value_accepts_less_than_and_markup`)

  Every accepted example must also parse with `extract_tool_calls` to the expected arguments.

  Verify with `cargo nextest run -p qmt-llama-cpp $F -E 'test(/schema_|value_accepts|every_detected_format/)'`.
  - Result: generators moved to `src/tools/lark.rs` (`qwen3_lark`, `qwen_function_lark`, `ministral_lark`, `lfm2_lark`, `function_gemma_lark`, `gemma4_lark`, each with `Tier`); `llg.rs` keeps the runtime and wires all six formats. Optional/required parameters and undeclared extras go through one `param_list` helper; undeclared names exclude the declared ones. The byte-level tests in `lark.rs` run every scenario over all six formats on a synthetic byte vocabulary with `0xFF` special delimiters, and check that each accepted call also parses with the provider's parser (`parse_assistant_format_with_tools`) to the expected arguments. The filter above: 13 passed (incl. `every_detected_format_compiles_under_llguidance` on the four real vocabularies, tier Schema, 0 fallbacks). Full package: 146 passed, 3 skipped.
  - Fix found on the way: the Qwen3 GBNF emitted tool names without JSON quotes (`"name": glob`), which `extract_tool_calls` cannot parse; `qwen3_tool_grammar` now quotes them (`"name": "glob"`), so both engines agree.

## 5. Real-vocabulary parity and triggers

- [ ] 5.1 Test local-tool-call-grammars / Generic grammar matches GBNF language (`generic_grammar_matches_gbnf_language`) and local-tool-call-grammars / Every format is checked on its model's tokenizer (`every_format_is_checked_on_its_models_tokenizer`: iterates all six formats over the tokenizers in the scenario and fails if any format has no real-tokenizer check): compare full-vocabulary masks of the generic llguidance tier and GBNF at several positions per format, using qwen35 for Qwen3, Qwen XML-function and Ministral, gemma-4 for Gemma 4, the LFM2 vocab for LFM2 and the FunctionGemma vocab for FunctionGemma. Exclude only byte tokens that are not standalone UTF-8.
- [ ] 5.2 Test local-tool-call-grammars / Special-token trigger activates the grammar (`special_token_trigger_activates_the_grammar`: gemma-4 `<|tool_call>`, LFM2 `<|tool_call_start|>`, FunctionGemma `<start_function_call>`, each on its real vocab) and local-tool-call-grammars / Special-token terminator closes the value (`special_token_terminator_closes_the_value`: gemma-4 `<|"|>`, FunctionGemma `<escape>`).
- [ ] 5.3 Test local-tool-call-grammars / Text before the trigger is unconstrained (`text_before_the_trigger_is_unconstrained`): before the trigger, the full-vocab mask allows every token for both backends.

## 6. Overhead benchmark

- [ ] 6.1 Convert `grammar_cost_benchmark` into the scenario test local-tool-call-grammars / Llguidance value tokens stay under one millisecond (`llguidance_value_tokens_stay_under_one_millisecond`):
  - keep it `#[ignore]` and gated by `QMT_RUN_GRAMMAR_BENCH=1`;
  - assert the llguidance mean is below 1 ms;
  - also print the one-time token-trie build time.

  Verify with `QMT_RUN_GRAMMAR_BENCH=1 cargo nextest run --release -p qmt-llama-cpp $F --run-ignored only --no-capture -E 'test(/llguidance_value_tokens/)'` and record the table in this task.

## 7. Docs and changelog

- [ ] 7.1 Document `grammar_backend` in `docs/docs/core/available-providers.md` (llama-cpp section): TOML path, values, default, fallback behaviour, schema-aware arguments.
- [ ] 7.2 Add CHANGELOG.md "Unreleased" entries:
  - **BREAKING:** llguidance default and schema-aware arguments in all formats.
  - **BREAKING:** new `LlamaCppConfig::grammar_backend` field.
  - Seeded output changes under rejection sampling.
  - Tool-call speed-up.

## 8. Integration checks

- [ ] 8.1 Run, in order:
  1. `cargo check -p qmt-llama-cpp $F --all-targets`
  2. `cargo nextest run -p qmt-llama-cpp $F`
  3. `cargo fmt -p qmt-llama-cpp -- --check`
  4. `cargo clippy -p qmt-llama-cpp $F --all-targets -- -D warnings`

  Record the counts. `cargo tree -i qmt-llama-cpp --workspace --depth 1` lists no dependents, so there is no downstream run.
- [ ] 8.2 Run `openspec validate add-llguidance-tool-grammars --strict` and verify that it passes.

## Workflow follow-up

- Manual check on the GPU host (dagon), CUDA plugin build:
  - rerun `qmt` test 3 and check that the GPUs stay busy during the `write_file` body;
  - run the arex-bench `probe` for arex2 and qwen38.
- Archive the change once review is done.
