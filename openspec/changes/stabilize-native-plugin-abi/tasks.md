# Tasks

Commands used below:
- `QM` = `cargo nextest run -p querymt --all-features`
- `FX` = `cargo nextest run -p querymt-native-plugin-fixture --features bench-support`
- `AG` = `QMT_UI_DIST=<minimal fixture from test-agent.yml> cargo nextest run -p querymt-agent --features dashboard,oauth,zbus-secret-service,systemd-credentials,remote`

## 1. Dependencies, features and ABI constants

- [ ] 1.1 In `crates/querymt/Cargo.toml`:
  - add `serde_bytes = "0.11"` (crate-local, always on);
  - add `rmp-serde = "1.3"` (optional);
  - add `rmp-serde` to the `native` feature;
  - add a new feature `native-plugin = ["dep:tokio", "dep:rmp-serde"]`.

  Verify with `cargo check -p querymt --all-features`, `cargo check -p querymt --no-default-features --features native-plugin` and `cargo build -p qmt-anthropic --release --target wasm32-wasip1`.
- [ ] 1.2 In `crates/querymt/src/plugin/mod.rs`, replace `NATIVE_PLUGIN_API` with `pub const NATIVE_PLUGIN_ABI: u32 = 3;` and add the empty modules `native_abi` (`cfg(any(feature = "native", feature = "native-plugin"))`) and `native_export` (`cfg(feature = "native-plugin")`). Verify with `cargo check -p querymt --all-features`; failures in the three native providers are expected until group 5.
- [ ] 1.3 Add `crates/querymt/build.rs` exporting `QMT_BUILD_VERSION` with the same rule as `crates/querymt-utils/build.rs`, and expose `querymt::BUILD_VERSION` and `querymt::is_release_version`. Verify with unit tests `release_version_is_plain_semver` and `nightly_and_describe_versions_are_not_releases` (`QM -E 'test(/release_version|are_not_releases/)'`).
- [ ] 1.4 Add the workspace member `crates/native-plugin-fixture`:
  - package `querymt-native-plugin-fixture`, `publish = false`, `crate-type = ["cdylib", "rlib"]`;
  - features `bench-support`;
  - dev-dependency `criterion = { version = "0.8", features = ["async_tokio"] }`;
  - `build.rs` records `FIXTURE_RUSTC` from `rustc -V`.

  Verify with `cargo check -p querymt-native-plugin-fixture --all-targets --features bench-support`.

## 2. Wire payloads

- [ ] 2.1 Add `#[serde(with = "serde_bytes")]` to `MediaSource::Inline::data` (`chat/output.rs`), `SttRequest::audio`, `TtsResponse::audio` and `VoiceConfig::Clone::reference_audio`. Add the tests:
  - `inline_media_bytes_keep_json_shape`: the JSON form is unchanged.
  - `msgpack_bytes_decode_from_bin_and_array`: rmp `bin` and int-array forms both decode, so old and new mesh peers stay compatible.

  Covers native-plugin-abi / Binary audio crosses the interface unchanged (encoding part). Verify with `QM -E 'test(/inline_media_bytes|msgpack_bytes_decode/)'`, then `cargo nextest run -p querymt-remote --all-features -E 'test(/provider_protocol/)'`.
- [ ] 2.2 In `plugin/native_abi.rs`, add:
  - the `#[repr(C)]` types `QmtBytes`, `QmtSink`, `QmtHostV3` and `QmtNativePluginV3`, with op and status codes;
  - the payload types `ChatRequestRef`/`ChatRequestOwned`, `OpenResult`, `Describe` and `StreamItem`;
  - the `encode`/`decode` helpers over `rmp_serde::to_vec_named`.

  Add the following tests, and check `testdata/native_wire_v3.txt` in:
  - `native_wire_encoding_snapshot`: a fixed set of requests, structured outputs, chunks and errors matches the checked-in hex;
  - `native_payload_decode_failure_names_abi`;
  - `error_payloads_round_trip_through_native_encoding`, covering native-plugin-abi / Rate-limit error keeps its retry delay (encoding part).

  Verify with `QM -E 'test(/native_wire|native_payload|error_payloads_round_trip_through_native/)'`.
- [ ] 2.3 Run `cargo fmt -p querymt -- --check` and `cargo clippy -p querymt --all-targets --all-features -- -D warnings`.

## 3. Plugin shim (`export_native_plugin!`)

- [ ] 3.1 Implement `plugin/native_export.rs`:
  - the per-library tokio runtime, created in `init`;
  - the forwarding `log::Log`, which replaces `plugin_log.rs`;
  - the provider handle registry;
  - `call` (synchronous decode, then spawn `catch_unwind`ed tasks), with the exactly-once `DoneGuard`;
  - `cancel` and `close`, and `emit == 1` as cancel;
  - `catch_unwind` around every `extern "C"` entry point;
  - `OPEN` via `spawn_blocking`;
  - the `export_native_plugin!` macro.

  Verify with `cargo check -p querymt --no-default-features --features native-plugin`.
- [ ] 3.2 Implement `FixtureFactory` in `crates/native-plugin-fixture/src/lib.rs`, with the request-driven behaviors from design §9. Export it through `export_native_plugin!` and statically through `create_factory()`. Verify with `cargo build -p querymt-native-plugin-fixture` and with `nm -gU` (or `dumpbin /exports`) showing `querymt_native_plugin_abi` and `querymt_native_plugin`.

## 4. Host loader and proxies

- [ ] 4.1 Rewrite `plugin/host/native.rs`:
  - the `NativeLibraries` leaked registry and handshake (ABI check, size check, `init` once);
  - error messages that recognize the old interface by probing its symbols;
  - the configured `plugin_api` check;
  - `NativeFactoryProxy`, which caches `DESCRIBE` and blocks on a sync receiver in `from_config`;
  - `NativeProviderProxy`, with call state, channels, `cancel` on drop and `close` on drop;
  - a stream proxy that reports a missing terminal chunk.

  Verify with `cargo check -p querymt --all-features`.
- [ ] 4.2 Rewrite the unit tests in `plugin/host/native.rs`, which compile one-line `rustc` fixtures, so they cover:
  - native-plugin-abi / Plugin reports a different ABI version (`plugin_reports_a_different_abi_version`);
  - native-plugin-abi / Library built for the previous native interface is rejected (`library_built_for_the_previous_native_interface_is_rejected`);
  - native-plugin-abi / Configured ABI differs from the host ABI (`configured_abi_differs_from_the_host_abi`).

  Verify with `QM -E 'test(/plugin_reports_a_different_abi|previous_native_interface|configured_abi_differs/)'`.
- [ ] 4.3 Add `crates/native-plugin-fixture/tests/support/mod.rs` with `fixture_library()` (honoring `QMT_NATIVE_FIXTURE_PATH`). In `tests/native_abi.rs`, add one test per scenario, loading through `PluginRegistry` and `NativeLoader`:
  - native-plugin-abi / Plugin built for the host ABI loads (`plugin_built_for_the_host_abi_loads`);
  - native-plugin-abi / Configured ABI equals the host ABI (`configured_abi_equals_the_host_abi`);
  - native-plugin-abi / Provider is created from a valid config (`provider_is_created_from_a_valid_config`);
  - native-plugin-abi / Plugin rejects a provider config (`plugin_rejects_a_provider_config`);
  - native-plugin-abi / Locked entry with a different factory name (`locked_entry_with_a_different_factory_name`);
  - native-plugin-abi / Chat result matches the in-process provider (`chat_result_matches_the_in_process_provider`);
  - native-plugin-abi / Binary audio crosses the interface unchanged (`binary_audio_crosses_the_interface_unchanged`);
  - native-plugin-abi / Operation not implemented by the plugin (`operation_not_implemented_by_the_plugin`);
  - chat-output-roundtrip / Structured output through a native plugin (`structured_output_through_a_native_plugin`).

  Verify with `FX -E 'test(/host_abi_loads|configured_abi_equals|valid_config|rejects_a_provider_config|different_factory_name|in_process_provider|binary_audio|not_implemented_by|structured_output_through/)'`.
- [ ] 4.4 Add stream, cancellation, error and runtime tests to `tests/native_abi.rs`:
  - native-plugin-abi / Stream delivers chunks in order (`stream_delivers_chunks_in_order`);
  - native-plugin-abi / Dropping a stream stops plugin generation (`dropping_a_stream_stops_plugin_generation`; the fixture records the cancellation it observed);
  - native-plugin-abi / Dropping an in-flight call cancels it (`dropping_an_in_flight_call_cancels_it`);
  - native-plugin-abi / Slow plugin call does not block the host executor (`slow_plugin_call_does_not_block_the_host_executor`, on a `current_thread` runtime);
  - native-plugin-abi / Rate-limit error keeps its retry delay (`rate_limit_error_keeps_its_retry_delay`);
  - native-plugin-abi / Panicking chat call (`panicking_chat_call`);
  - native-plugin-abi / Panic while streaming (`panic_while_streaming`);
  - native-plugin-abi / Provider spawns async tasks (`provider_spawns_async_tasks`, called from a tokio task and from `std::thread`).

  Verify with `FX -E 'test(/chunks_in_order|stops_plugin_generation|in_flight_call|block_the_host_executor|retry_delay|panicking_chat|panic_while_streaming|spawns_async_tasks/)'`.
- [ ] 4.5 Add logging and lifetime tests to `tests/native_abi.rs`:
  - native-plugin-abi / Plugin warning appears in host logs (`plugin_warning_appears_in_host_logs`, with a capturing `log::Log`);
  - native-plugin-abi / Records above the host level are not forwarded (`records_above_the_host_level_are_not_forwarded`);
  - native-plugin-abi / Provider outlives its factory (`provider_outlives_its_factory`);
  - native-plugin-abi / Same library configured twice (`same_library_configured_twice`; the fixture counts `init` calls).

  Verify with `FX -E 'test(/host_logs|above_the_host_level|outlives_its_factory|configured_twice/)'`.
- [ ] 4.6 Add the ignored test `plugin_built_with_a_different_compiler_release`, covering native-plugin-abi / Plugin built with a different compiler release. It requires `QMT_NATIVE_FIXTURE_PATH`, asserts that the plugin's `FIXTURE_RUSTC` differs from the test binary's, and runs a chat, a stream, an error and a log through the plugin. Verify locally with:

  ```
  cargo +beta build -p querymt-native-plugin-fixture --release
  QMT_NATIVE_FIXTURE_PATH=<artifact> cargo +stable nextest run -p querymt-native-plugin-fixture --run-ignored only -E 'test(/different_compiler_release/)'
  ```

  CI runs it in 9.1.
- [ ] 4.7 Run `cargo fmt -p querymt -p querymt-native-plugin-fixture -- --check`, `cargo clippy -p querymt --all-targets --all-features -- -D warnings` and `cargo clippy -p querymt-native-plugin-fixture --all-targets --features bench-support -- -D warnings`.

## 5. Migrate providers and remove the old surface

- [ ] 5.1 In `crates/providers/{llama-cpp,mrs,izwi}`:
  - replace the hand-written exports with `querymt::export_native_plugin!(…)`;
  - remove `plugin_init_logging`;
  - make each `native` feature enable `querymt/native-plugin`.

  Verify with `cargo check -p qmt-<name> --no-default-features --features native` for each provider, the `nm` export check from 3.2 on `cargo build -p qmt-llama-cpp --release`, and `cargo clippy -p qmt-<name> --all-targets --no-default-features --features native -- -D warnings`. mrs tests are not run because they download models.
- [ ] 5.2 Remove the `plugin_http_factory` exports from the 14 HTTP provider crates (alibaba, anthropic, codex, deepseek, google, groq, kimi-code, mistral, moonshot, ollama, openai, openrouter, xai, zai). Verify with `cargo check -p qmt-<name> --no-default-features --features native` and `cargo build -p qmt-<name> --release --target wasm32-wasip1` for each, and with `cargo nextest run -p qmt-openai --no-default-features --features native` (statically linked tests still pass).
- [ ] 5.3 Delete from `querymt::plugin`: `FactoryCtor`, `HTTPFactoryCtor`, `LogCallbackFn`, `PluginInitLoggingFn`, the `plugin_log` module, `NativeFactoryWrapper` and `NATIVE_PLUGIN_API`. Verify that `rg 'plugin_http_factory|plugin_init_logging|querymt_plugin_api|NATIVE_PLUGIN_API|FactoryCtor' crates docs` returns nothing, then run `cargo check -p querymt --all-features`.
- [ ] 5.4 Rewrite the native sections of `docs/docs/plugins/interface_spec.md` and `docs/docs/plugins/development.md` around `export_native_plugin!`:
  - the ABI 3 rule;
  - `panic = "unwind"` is required;
  - HTTP providers are Wasm or statically linked.

  Add the native-interface and HTTP-native removal entries under "Unreleased" in `CHANGELOG.md`, marked **BREAKING**. Verify with `mkdocs build --strict` from `docs/`.

## 6. Host artifact resolution and ABI pre-check

- [ ] 6.1 Add `plugin/host/reference.rs` `resolve_oci_reference` (moving `expected_scalar_feature` there) and apply it in `PluginRegistry::load_and_process_plugin` and the prefetch loop. Tests:
  - plugin-artifact-distribution / Release host resolves its own release (`release_host_resolves_its_own_release`);
  - plugin-artifact-distribution / Release host without a feature (`release_host_without_a_feature`);
  - plugin-artifact-distribution / Development host resolves edge (`development_host_resolves_edge`);
  - plugin-artifact-distribution / Explicit tag is kept (`explicit_tag_is_kept`);
  - plugin-artifact-distribution / Explicit digest is kept (`explicit_digest_is_kept`);
  - plugin-artifact-distribution / Several features on an untagged reference (`several_features_on_an_untagged_reference`; the registry-level variant asserts that other providers still load).

  Verify with `QM -E 'test(/resolves_its_own_release|without_a_feature|resolves_edge|explicit_tag_is_kept|explicit_digest_is_kept|several_features/)'`.
- [ ] 6.2 In `oci.rs`, add `check_native_manifest_abi` and `fetch_plugin_layer(plugin_type, annotations, fetch)`. Call them from `pull_and_extract` before the layer download, and from `load_from_cache`. Tests (the download closure panics if called):
  - plugin-artifact-distribution / Artifact built for another ABI (`artifact_built_for_another_abi`);
  - plugin-artifact-distribution / Native artifact without an ABI annotation (`native_artifact_without_an_abi_annotation`);
  - plugin-artifact-distribution / Cached artifact from an older host (`cached_artifact_from_an_older_host`).

  Update `test_load_from_cache_native` with an ABI 3 annotation. Verify with `QM -E 'test(/another_abi|without_an_abi_annotation|older_host|load_from_cache/)'`.
- [ ] 6.3 Add a test for native-plugin-abi / Configured ABI is not an unsigned integer (`configured_abi_is_not_an_unsigned_integer`) in `plugin/host/config.rs`, covering `-1` and `"3"`. Add the same test name in `crates/agent/src/profiles.rs` for `[profile.providers.<name>] plugin_api`. Verify with `QM -E 'test(/configured_abi_is_not/)'` and `AG -E 'test(/configured_abi_is_not/)'`.
- [ ] 6.4 Document `plugin_api` (type, default, allowed value 3, error on mismatch) and the untagged-reference rule with `features` in `docs/docs/plugins/configuration.md` and `docs/docs/plugins/oci_plugins.md`. Switch `providers.example.toml` and the native recipes in `docs/docs/core/available-providers.md` to untagged references plus `features`. Add the untagged-reference **BREAKING** entry to `CHANGELOG.md`. Verify with `mkdocs build --strict` and a test, `providers_example_config_parses`, that loads `providers.example.toml` (`QM -E 'test(/providers_example_config_parses/)'`).
- [ ] 6.5 Run `cargo fmt -p querymt -p querymt-agent -- --check`, `cargo clippy -p querymt --all-targets --all-features -- -D warnings` and the `querymt-agent` clippy run with the CI features and `QMT_UI_DIST`, with `-D warnings`.

## 7. Publishing: tags, annotations, provider index

- [ ] 7.1 Add the scripts `scripts/release/plugin-tags.sh` (including `--check-release-ref`), `native-plugin-abi.sh` and `provider-index.sh`, with cases in `scripts/release/test-release-scripts.sh`:
  - plugin-artifact-distribution / Native release build tags (`native_release_build_tags`);
  - plugin-artifact-distribution / Wasm release build tags (`wasm_release_build_tags`);
  - plugin-artifact-distribution / Release tag does not match the crate version (`release_tag_does_not_match_the_crate_version`);
  - plugin-artifact-distribution / Main build after a release (`main_build_after_a_release`);
  - plugin-artifact-distribution / Wasm main build tags (`wasm_main_build_tags`);
  - plugin-artifact-distribution / Release provider index (`release_provider_index`);
  - plugin-artifact-distribution / Development provider index (`development_provider_index`);
  - plugin-artifact-distribution / Annotations match the built library (`annotations_match_the_built_library`: `native-plugin-abi.sh` prints 3 for the real `plugin/mod.rs` and fails on a file without the constant).

  Verify with `scripts/release/test-release-scripts.sh`.
- [ ] 7.2 Rewire `.github/actions/native-publish/action.yml`, `.github/workflows/native-create-manifests.yml` (index creation, signing, validation, plus a final `cosign verify` of every listed tag) and `.github/workflows/publish-providers.yml` (Wasm push, signing, provider index) to the scripts. Add the annotations `mt.query.plugin.api=$(native-plugin-abi.sh)` and `mt.query.plugin.revision=${GITHUB_SHA}`. Add a test-release-scripts case, `no_stable_alias_after_a_release`, that fails if any workflow or action still writes a `stable` alias or an unversioned per-platform tag (plugin-artifact-distribution / No stable alias after a release). Verify with `scripts/release/test-release-scripts.sh`. plugin-artifact-distribution / Edge alias is signed is verified by the `cosign verify` step on the first `main` run after merge; record the run URL in the PR.
- [ ] 7.3 Update the tag tables and channel text in `docs/docs/core/available-providers.md` (`latest*` = newest release, `edge*` = `main`, `<version>-dev.<sha7>*`, `stable*` retired) and in `docs/docs/core/providers.md`. Add the tag-channel **BREAKING** entry to `CHANGELOG.md`. Verify with `mkdocs build --strict`.

## 8. Benchmark target

- [ ] 8.1 Implement `crates/native-plugin-fixture/src/bench_support.rs`:
  - the case table: groups `latency`, `stream`, `concurrent_stream`, `bandwidth` with the case names and sizes from the spec;
  - payload builders;
  - `run_case_once(case, path)`;
  - `check_paths_equal(case)`.

  Add `benches/native_abi.rs`: criterion, `harness = false`, the `[[bench]] name = "native_abi"` entry with `required-features = ["bench-support"]`, and IDs in the form `<group>/<native|in_process>/<case>` with `Throughput::Elements`/`Bytes`. Verify with `cargo bench -p querymt-native-plugin-fixture --features bench-support --bench native_abi -- --test`.
- [ ] 8.2 Add tests in `tests/bench_cases.rs`:
  - native-plugin-benchmarks / Benchmark runs offline without models (`benchmark_runs_offline_without_models`: every case runs once on both paths using only the fixture, with no model, network or GPU code);
  - native-plugin-benchmarks / Paths are reported side by side (`paths_are_reported_side_by_side`: every case yields exactly the two IDs);
  - native-plugin-benchmarks / Paths return identical results (`paths_return_identical_results`: a deliberately mismatched result makes `check_paths_equal` fail, naming the case);
  - native-plugin-benchmarks / Latency of a small chat call (`latency_of_a_small_chat_call`);
  - native-plugin-benchmarks / Latency to the first stream chunk (`latency_to_the_first_stream_chunk`: the timed future resolves after the first chunk and cancels the rest);
  - native-plugin-benchmarks / Throughput of a long stream (`throughput_of_a_long_stream`: 100 000 chunks received on both paths);
  - native-plugin-benchmarks / Throughput of sixteen concurrent streams (`throughput_of_sixteen_concurrent_streams`: 160 000 chunks in total);
  - native-plugin-benchmarks / Bandwidth of a large binary request (`bandwidth_of_a_large_binary_request`: the plugin receives the 16 MiB unchanged);
  - native-plugin-benchmarks / Bandwidth of a large text response (`bandwidth_of_a_large_text_response`: the host receives the 16 MiB unchanged).

  Verify with `FX -E 'test(/offline_without_models|side_by_side|identical_results|latency_of|latency_to|throughput_of|bandwidth_of/)'`.
- [ ] 8.3 Run the full benchmark once (`cargo bench -p querymt-native-plugin-fixture --features bench-support --bench native_abi`). Record native vs in-process numbers per group in design.md, and resolve the open question on stream batching from them. Verify that design.md contains the results table.

## 9. CI

- [ ] 9.1 Add a `native-abi` job to `.github/workflows/test-querymt.yml`, and add `crates/native-plugin-fixture/**` and `scripts/release/**` to its path filters. The job:
  - runs `FX`;
  - runs `cargo bench -p querymt-native-plugin-fixture --features bench-support --bench native_abi -- --test`, covering native-plugin-benchmarks / Quick mode in CI;
  - installs stable and beta, builds the fixture with `cargo +beta build --release`, and runs `plugin_built_with_a_different_compiler_release` under stable with `QMT_NATIVE_FIXTURE_PATH`.

  Verify on the PR run that the job is green, and that a deliberately broken bench case (a local experiment, not committed) turns it red.

## 10. Unchanged chat-output-roundtrip scenarios and integration

- [ ] 10.1 Confirm that the unchanged scenarios of the modified requirement still pass with their existing tests:
  - chat-output-roundtrip / Older remote or plugin peer (`requests_advertise_item_aware_contract_only_when_needed` in querymt-remote, and `plugin_built_against_the_previous_contract_asks_for_a_rebuild` in querymt);
  - chat-output-roundtrip / Existing stored history (`complete_old_history_normalizes_resaves_and_exports_without_hidden_state`, `old_message_json_normalizes_to_canonical_payload`);
  - chat-output-roundtrip / Canonical portable history targets an older peer (`portable_output_strips_native_state_but_keeps_call_identity`);
  - chat-output-roundtrip / Item-aware state targets an older peer (`requests_advertise_item_aware_contract_only_when_needed`).

  If reading one of these tests shows it does not assert the scenario's THEN outcome, add a test named after the scenario next to it. Verify with `QM -E 'test(/previous_contract_asks|complete_old_history|old_message_json|portable_output_strips/)'` and `cargo nextest run -p querymt-remote --all-features -E 'test(/requests_advertise_item_aware/)'`.
- [ ] 10.2 Check the downstream packages from `cargo tree -i querymt --workspace --depth 1`:
  - `cargo check` for `querymt-remote`, `querymt-cli`, `querymt-mobile-ffi` and `querymt-extism-macros`;
  - `cargo check` for `querymt-py` with `PYO3_PYTHON` set to Python 3.10 or newer;
  - `cargo check -p querymt-agent` with its CI features and `QMT_UI_DIST`;
  - `cargo nextest run -p querymt-remote --all-features`;
  - `cargo build --release --target wasm32-wasip1` for `qmt-openai` and `qmt-anthropic`.

  No workspace-wide runs.
- [ ] 10.3 Run `openspec validate stabilize-native-plugin-abi --strict` and confirm it reports the change as valid.

## Workflow follow-up

- After merge, check that the first `main` run publishes only `edge*` and `*-dev.<sha7>*` tags with `mt.query.plugin.api=3`, and that the next release tag publishes `<version>*` and `latest*`.
- Archive the change once released.
