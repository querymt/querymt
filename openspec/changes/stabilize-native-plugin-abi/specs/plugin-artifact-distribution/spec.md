# Spec Delta

## Purpose

Defines which OCI tags and annotations release and development builds publish for provider plugins, how the host turns a provider reference into an artifact, and how it rejects native artifacts built for another ABI, so that a released host gets plugins from its own release.

## ADDED Requirements

### Requirement: Release builds publish version tags and the latest aliases
A build triggered by a release git tag `v<version>` SHALL publish each plugin under these tags, all pointing at that release's artifacts:
- native plugins: `<version>`, `<version>-<feature>`, `<version>-<os>-<arch>-<feature>`, `latest` and `latest-<feature>`;
- Wasm plugins: `<version>` and `latest`.

The publish SHALL fail when `<version>` differs from the plugin crate's version.

#### Scenario: Native release build tags
- **WHEN** the release tag `v0.6.0` builds llama-cpp with feature tag `metal` for darwin/arm64
- **THEN** the registry has `llama-cpp:0.6.0-metal`, `llama-cpp:latest-metal` and `llama-cpp:0.6.0-darwin-arm64-metal`, all from that build.

#### Scenario: Wasm release build tags
- **WHEN** the release tag `v0.6.0` builds the anthropic Wasm plugin
- **THEN** the registry has `anthropic:0.6.0` and `anthropic:latest` pointing at that artifact.

#### Scenario: Release tag does not match the crate version
- **WHEN** the release tag `v0.6.1` builds a plugin whose crate version is `0.6.0`
- **THEN** the publish fails before pushing any tag.

### Requirement: Development builds never move release tags
A plugin build from the `main` branch SHALL publish only:
- the immutable tags `<version>-dev.<sha7>`, `<version>-dev.<sha7>-<feature>` and `<version>-dev.<sha7>-<os>-<arch>-<feature>`, where `<sha7>` is the first seven characters of the commit hash;
- the moving aliases `edge` and `edge-<feature>`.

It SHALL NOT write any `<version>`, `<version>-<feature>`, `latest`, `latest-<feature>` or `stable*` tag.

#### Scenario: Main build after a release
- **WHEN** commit `abc1234…` on `main` rebuilds llama-cpp, crate version `0.6.0`, after `v0.6.0` was released
- **THEN** the registry gains `llama-cpp:0.6.0-dev.abc1234-metal` and `llama-cpp:edge-metal`
- **AND** `llama-cpp:0.6.0-metal` and `llama-cpp:latest-metal` still point at the release build.

#### Scenario: Wasm main build tags
- **WHEN** commit `abc1234…` on `main` rebuilds the anthropic Wasm plugin, crate version `0.6.0`
- **THEN** the registry gains `anthropic:0.6.0-dev.abc1234` and `anthropic:edge`, and `anthropic:0.6.0` and `anthropic:latest` are unchanged.

### Requirement: Published tags are signed and stable aliases are retired
Every tag that a release or development build writes SHALL point at a cosign-signed manifest. Builds SHALL no longer publish `stable`, `stable-<feature>`, or per-platform tags without a version (`<os>-<arch>-<feature>`).

#### Scenario: Edge alias is signed
- **WHEN** a `main` build publishes `llama-cpp:edge-metal`
- **THEN** the manifest that tag points at has a cosign signature from the publishing workflow.

#### Scenario: No stable alias after a release
- **WHEN** the release tag `v0.6.0` builds and publishes plugins
- **THEN** no `stable` or `stable-<feature>` tag is written for any plugin.

### Requirement: The provider index follows the same channels
The Wasm provider index SHALL reference only tags of its own channel:
- A release build SHALL write `stable.json` and `<version>.json`, whose entries reference `oci://ghcr.io/<owner>/<provider>:<version>`.
- A `main` build SHALL write `latest.json`, whose entries reference `oci://ghcr.io/<owner>/<provider>:edge`.

#### Scenario: Release provider index
- **WHEN** the release tag `v0.6.0` publishes the provider index
- **THEN** every entry in `stable.json` and `0.6.0.json` references its provider at tag `0.6.0`.

#### Scenario: Development provider index
- **WHEN** a `main` build publishes the provider index
- **THEN** every entry in `latest.json` references its provider at tag `edge`.

### Requirement: Native artifacts declare their ABI and source revision
Every published native plugin manifest SHALL carry the annotation `mt.query.plugin.api`, set to the native plugin ABI version of the library in that manifest, and the annotation `mt.query.plugin.revision`, set to the full source commit hash it was built from. The ABI value SHALL come from the source being built; the publish SHALL fail if it cannot be determined.

#### Scenario: Annotations match the built library
- **WHEN** a native plugin is published from a commit whose native plugin ABI version is 3
- **THEN** its manifest has `mt.query.plugin.api=3` and `mt.query.plugin.revision` equal to that commit hash.

### Requirement: Untagged provider references resolve to the host's channel
For a `[[providers]] path` of the form `oci://<registry>/<repository>` with neither tag nor digest, the host SHALL choose the tag from its own build version and the entry's `features` (list of strings, default empty):
- A release host (version `X.Y.Z`) SHALL use tag `X.Y.Z`, or `X.Y.Z-<feature>` when a feature is set.
- Any other host build, such as nightly or local development, SHALL use `edge` or `edge-<feature>`.

#### Scenario: Release host resolves its own release
- **WHEN** a host built as release `0.6.0` loads `path = "oci://ghcr.io/querymt/llama-cpp"` with `features = ["metal"]`
- **THEN** it pulls `ghcr.io/querymt/llama-cpp:0.6.0-metal`.

#### Scenario: Release host without a feature
- **WHEN** a host built as release `0.6.0` loads `path = "oci://ghcr.io/querymt/anthropic"` with no `features`
- **THEN** it pulls `ghcr.io/querymt/anthropic:0.6.0`.

#### Scenario: Development host resolves edge
- **WHEN** a host built as `0.6.1-nightly.20261112030000` loads `path = "oci://ghcr.io/querymt/llama-cpp"` with `features = ["cuda12.9"]`
- **THEN** it pulls `ghcr.io/querymt/llama-cpp:edge-cuda12.9`.

### Requirement: Explicit tags and digests are used unchanged
A `[[providers]] path` OCI reference that carries a tag or a digest SHALL be pulled exactly as written, whatever the host's version and the entry's `features`.

#### Scenario: Explicit tag is kept
- **WHEN** any host loads `path = "oci://ghcr.io/querymt/llama-cpp:0.6.0-dev.abc1234-metal"`
- **THEN** it pulls exactly that tag.

#### Scenario: Explicit digest is kept
- **WHEN** a host built as release `0.6.0` loads a reference pinned with `@sha256:<digest>`
- **THEN** it pulls exactly that digest and does not substitute a release tag.

### Requirement: Untagged references accept at most one feature
For an untagged OCI reference, `features` SHALL contain at most one entry. More entries SHALL make that provider fail to load with an error naming the features, while other providers still load.

#### Scenario: Several features on an untagged reference
- **WHEN** a provider entry with an untagged reference sets `features = ["metal", "vulkan"]`
- **THEN** that provider fails to load with an error naming both features, and other providers still load.

### Requirement: Native artifacts for another ABI are rejected before download
The host SHALL check the `mt.query.plugin.api` annotation of a native plugin manifest against its own native plugin ABI version before downloading the library. A missing or different value SHALL fail that provider with an error naming the reference, the artifact's ABI (or its absence) and the host's ABI. The library SHALL NOT be downloaded or loaded. A cached native artifact SHALL be checked the same way before use. Wasm artifacts are not subject to this check.

#### Scenario: Artifact built for another ABI
- **WHEN** a host with native ABI 3 resolves a native manifest annotated `mt.query.plugin.api=2`
- **THEN** loading fails with an error naming the reference, "2" and "3"
- **AND** no library layer is downloaded.

#### Scenario: Native artifact without an ABI annotation
- **WHEN** a host resolves a native manifest that has no `mt.query.plugin.api` annotation
- **THEN** loading fails with an error saying the artifact declares no native plugin ABI, and no library layer is downloaded.

#### Scenario: Cached artifact from an older host
- **WHEN** the plugin cache holds a native artifact annotated `mt.query.plugin.api=2` and a host with ABI 3 loads the same reference while the registry is unreachable
- **THEN** loading fails with the ABI mismatch error instead of loading the cached library.
