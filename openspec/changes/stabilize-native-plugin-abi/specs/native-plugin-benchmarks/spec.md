# Spec Delta

## Purpose

Provides a repeatable benchmark target that measures the latency, throughput and bandwidth of the native plugin interface against an in-process baseline, so the cost of the interface stays known and regressions are visible.

## ADDED Requirements

### Requirement: Native interface benchmark runs on any development machine
The workspace SHALL provide a benchmark target, run with `cargo bench -p querymt-native-plugin-fixture --bench native_abi`. It SHALL measure the native plugin interface using a test plugin built from the workspace. It SHALL NOT need model files, a GPU, network access or credentials, and SHALL run on Linux, macOS and Windows.

#### Scenario: Benchmark runs offline without models
- **WHEN** the benchmark target is run on a machine with no network access and no model files
- **THEN** every benchmark case completes and reports its measurements.

### Requirement: Each case is measured through the native interface and in process
Every benchmark case SHALL be measured on two paths that run the same provider implementation with the same payloads: `native` (the plugin library loaded through the native interface) and `in_process` (the same provider linked into the benchmark). Each reported measurement SHALL name its group, its path and its case, in the form `<group>/<path>/<case>`.

#### Scenario: Paths are reported side by side
- **WHEN** the benchmark completes
- **THEN** every case appears once as `<group>/native/<case>` and once as `<group>/in_process/<case>`.

#### Scenario: Paths return identical results
- **WHEN** a benchmark case starts
- **THEN** it first checks that the native and in-process paths return equal results for its payload
- **AND** it aborts with an error naming the case if they differ, instead of reporting timings.

### Requirement: Call latency is measured
The `latency` group SHALL report the time of one complete round trip for these cases:
- `chat_small`: a non-streaming chat call with a short text request and a short text result.
- `stream_first_chunk`: a streamed chat call, timed until its first chunk.
- `not_implemented`: a call to an operation the provider does not implement.

#### Scenario: Latency of a small chat call
- **WHEN** the `latency` group runs
- **THEN** it reports the time per call for `latency/native/chat_small` and `latency/in_process/chat_small`.

#### Scenario: Latency to the first stream chunk
- **WHEN** the `latency` group runs
- **THEN** it reports the time from starting a streamed chat until its first chunk arrives, for both paths.

### Requirement: Stream throughput is measured
The `stream` group SHALL report how many chunks per second one streamed chat delivers to the host, for streams of 100, 10 000 and 100 000 small text chunks. The cases SHALL be named `chunks_100`, `chunks_10000` and `chunks_100000`, and each SHALL be reported in elements per second.

#### Scenario: Throughput of a long stream
- **WHEN** the `stream` group runs the `chunks_100000` case
- **THEN** it reports chunks per second for both paths, counting every chunk the host received.

### Requirement: Concurrent stream throughput is measured
The `concurrent_stream` group SHALL report total chunks per second when 1, 4 and 16 streamed chats of 10 000 chunks each run at the same time against one provider. The cases SHALL be named `streams_1`, `streams_4` and `streams_16`.

#### Scenario: Throughput of sixteen concurrent streams
- **WHEN** the `concurrent_stream` group runs the `streams_16` case
- **THEN** it reports the total chunks per second received by the host across all 16 streams, for both paths.

### Requirement: Payload bandwidth is measured
The `bandwidth` group SHALL report bytes per second for payloads of 1 KiB, 64 KiB, 1 MiB and 16 MiB, in four directions:
- `request_text`: chat request text sent to the provider.
- `response_text`: chat result text returned to the host.
- `request_binary`: transcription audio sent to the provider.
- `response_binary`: speech audio returned to the host.

Cases SHALL be named `<direction>_<size>`, for example `request_binary_16MiB`.

#### Scenario: Bandwidth of a large binary request
- **WHEN** the `bandwidth` group runs `request_binary_16MiB`
- **THEN** it reports bytes per second for sending 16 MiB of audio to the provider, for both paths.

#### Scenario: Bandwidth of a large text response
- **WHEN** the `bandwidth` group runs `response_text_16MiB`
- **THEN** it reports bytes per second for returning 16 MiB of chat text to the host, for both paths.

### Requirement: Benchmark cases are smoke-tested in CI
CI SHALL run every benchmark case once in quick mode (`cargo bench -p querymt-native-plugin-fixture --bench native_abi -- --test`) on Linux, so a benchmark that no longer builds, panics or reports mismatched results fails the build. CI SHALL NOT compare timings against thresholds.

#### Scenario: Quick mode in CI
- **WHEN** CI runs the benchmark target in quick mode
- **THEN** every case runs once, including the result equality checks
- **AND** the job fails if any case fails to run or its results differ between paths.
