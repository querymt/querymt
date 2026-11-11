# Tasks

## 1. Anthropic breakpoint budget (qmt-anthropic)

- [ ] 1.1 Add the private `normalize_cache_breakpoints` function in `crates/providers/anthropic/src/lib.rs` (design D1/D2). It collects wire blocks whose `cache_control` is `Some`, across system blocks and message blocks. It ranks them as last system, last message, second-to-last message, first system, then other messages newest first, then other system markers last first. It keeps the top four, clears the rest and returns the number dropped. Verify with `cargo check -p qmt-anthropic --no-default-features --features native`.
- [ ] 1.2 Add lifetime normalization after dropping (design D3). Walk the kept markers from the end of the request to the start and raise each one to the longest lifetime that follows it, ranking `1h` above `5m` and default. Covers `anthropic-messages-protocol` / Default system marker before a one-hour marker is raised and / Longer lifetime before shorter is preserved. Tests: `default_system_marker_before_a_one_hour_marker_is_raised` and `longer_lifetime_before_shorter_is_preserved`.
- [ ] 1.3 Call the function in `Anthropic::build_chat_request` after the OAuth billing block is inserted and before `AnthropicCompleteRequest` is built. Emit one `log::warn!` with the dropped count when it is greater than zero. Add tests in the crate's `mod tests` that go through `build_chat_request` and count `cache_control` in the serialized body. Each test is named after the scenario it covers in `anthropic-messages-protocol`:
  - / Five requested breakpoints are reduced to four: `five_requested_breakpoints_are_reduced_to_four`
  - / Requests within the limit are unchanged: `requests_within_the_limit_are_unchanged`
  - / Hints that place no marker do not use the budget: `hints_that_place_no_marker_do_not_use_the_budget`
  - / Middle system markers are dropped first: `middle_system_markers_are_dropped_first`. Also check the returned dropped count is 2. The warning itself is checked by review, because the crate has no log capture.
  - / Older history markers are dropped before recent ones: `older_history_markers_are_dropped_before_recent_ones`
  - / OAuth identity blocks do not shift system priority: `oauth_identity_blocks_do_not_shift_system_priority`
  
  Verify with `cargo nextest run -p qmt-anthropic --no-default-features --features native -E 'test(/breakpoint|markers|marker_|oauth_identity_blocks|requests_within_the_limit|lifetime/)'`.
- [ ] 1.4 Run the remaining checks for qmt-anthropic:
  - `cargo nextest run -p qmt-anthropic --no-default-features --features native`
  - `cargo fmt -p qmt-anthropic -- --check`
  - `cargo clippy -p qmt-anthropic --no-default-features --features native --all-targets -- -D warnings`
  - `cargo build -p qmt-anthropic --release --target wasm32-wasip1`, to confirm the plugin build.
- [ ] 1.5 Add an "Anthropic" paragraph in `docs/docs/core/available-providers.md`, under `### anthropic`. It covers the four-breakpoint limit, the keep priority, the single warning and the TTL raise. Verify by reading the rendered section with `mkdocs serve`, or by reviewing the markdown if mkdocs is unavailable.

## 2. Agent default placement (querymt-agent)

- [ ] 2.1 Change `ModelDefaults::apply_system_cache_defaults` in `crates/agent/src/model_heuristics.rs` so default `cache` is added only at indices `0` and `len-1`. Existing `cache` (including `null`) and `cache_control` must stay untouched at every position (design D4). Verify with `cargo check -p querymt-agent --features oauth,remote`.
- [ ] 2.2 Add tests in `model_heuristics.rs` `mod tests` through `ModelDefaults::apply_to`, each named after its `agent-prompt-caching` scenario:
  - / Four sections get two default hints: `four_sections_get_two_default_hints`
  - / Single section gets one default hint: `single_section_gets_one_default_hint`. This can extend the existing `test_anthropic_system_caching_string`.
  - / Providers without cache defaults are untouched: `providers_without_cache_defaults_are_untouched`, which uses openai without `api_mode`.
  - / Explicit middle hint is kept: `explicit_middle_hint_is_kept`
  - / Explicit null opts the last section out: `explicit_null_opts_the_last_section_out`. The test should deserialize the result into `Vec<SystemPrompt>` and assert section 2 has no hint.
  
  Update existing tests whose expectations assumed every section is hinted. Verify with `cargo nextest run -p querymt-agent --features oauth,remote -E 'test(/default_hint|cache_defaults|section|system_caching|explicit_/)'`.
- [ ] 2.3 Add `dotagents_sections_count_as_system_sections` in `crates/agent/src/dotagents/tests.rs`. It builds `LLMParams` with one system file, applies `compose_prompt` with a manifest holding `SYSTEM.md` and `AGENTS.md`, then runs `ModelDefaults::apply_to` for anthropic. It asserts that only the first and last sections carry hints. Covers `agent-prompt-caching` / Dotagents sections count as system sections. Verify with `cargo nextest run -p querymt-agent --features oauth,remote -E 'test(/dotagents_sections_count_as_system_sections/)'`.
- [ ] 2.4 Name the existing `apply_cache_breakpoints` tests in `crates/agent/src/agent/execution/transitions.rs` after their scenarios, adding them where they are missing:
  - `agent-prompt-caching` / Last two messages are hinted: rename `test_cache_breakpoints_five_messages_only_last_two_cached` to `last_two_messages_are_hinted`.
  - `agent-prompt-caching` / Existing history hints are kept: add `existing_history_hints_are_kept` with a hinted message 1 of five, and include an idempotence assertion.
  
  Verify with `cargo nextest run -p querymt-agent --features oauth,remote -E 'test(/last_two_messages_are_hinted|existing_history_hints_are_kept/)'`.
- [ ] 2.5 Rewrite the "Prompt Cache Hints" paragraph in `docs/docs/core/providers.md`, replacing "each instruction section is a prefix boundary" with:
  - the first/last default rule;
  - explicit hints for middle sections;
  - a link to the Anthropic limit.
  
  Add an Unreleased **BREAKING** entry under "Behavior changes" in `CHANGELOG.md` covering the narrower defaults and the Anthropic cap and TTL raise. Verify by review of both files.
- [ ] 2.6 Run the remaining checks for querymt-agent with the CI features and `QMT_UI_DIST` set to the minimal fixture from `.github/workflows/test-agent.yml`:
  - `cargo nextest run -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote -E 'test(/model_heuristics|cache|dotagents/)'`
  - `cargo fmt -p querymt-agent -- --check`
  - `cargo clippy -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote --all-targets -- -D warnings`
  
  The zbus and systemd paths are unverified on macOS.

## 3. Integration

- [ ] 3.1 Check the downstream packages: run `cargo check -p querymt-mobile-ffi` (from `cargo tree -i qmt-anthropic`) and confirm it builds unchanged.
- [ ] 3.2 Run `openspec validate enforce-anthropic-cache-breakpoint-budget --strict` and confirm it passes with no warnings.
