---
id: agent-profiles
title: Agent profiles
description: How named agent profiles are cataloged, validated, materialized into runtimes, hot-reloaded and bound to sessions, including provider-locked profiles.
tags: profiles, catalog, runtime, session-binding, providers, reload
knowledge_paths: ["crates/agent/src/profiles.rs", "crates/agent-runtime/src/profile_host.rs"]
knowledge_sources: ["crates/agent/src/profiles.rs", "crates/agent-runtime/src/profile_host.rs"]
---

# Agent profiles

A profile is a named agent configuration. Its body is an ordinary single-agent or quorum
TOML config (loaded by `crates/agent-runtime/src/config/loader.rs`); the profile layer adds
an id, catalog metadata, optional provider requirements and a per-session binding. This note
is about the profile layer only. User-facing usage is in `docs/docs/agent/profiles.md`.

## Where the code lives

The data types, the `ProfileCatalog` trait and the object-safe `ProfileRuntimeHost` trait are
in `crates/agent-runtime/src/profile_host.rs`. The catalog implementation
(`LocalProfileCatalog`, `ProfileCatalogBuilder`) and the manager (`ProfileRuntimeManager`) are
in `crates/agent/src/profiles.rs`, because materializing a profile builds an `api::Agent`,
which the runtime crate cannot depend on. Runtime code sees a materialized profile only as a
`ProfileRuntimeView` (root handle, delegate handles, provider lock).

## Profile file format

A profile file is a config file plus an optional top-level `[profile]` table with `id`, `name`,
`description`, `tags`, `provider_mode` and `providers` (`ProfileFileMetadata`). The config
loader removes the `profile` table before its strict deserialization
(`build_config_from_toml_value` in `crates/agent-runtime/src/config/loader.rs`), so catalog
metadata can never change agent behavior while every other unknown field is still rejected.
Whether a profile is `Single` or `Quorum` is inferred from the presence of `[agent]` versus
`[quorum]`/`[planner]` (`infer_config_kind_from_toml`) and re-derived from the loaded config.

## Catalog sources and listing

`ProfileSource` is `Embedded { key }` (only the `default` profile, loaded from a checked-in
file path), `EmbeddedToml { key }` (TOML compiled into the binary) or `LocalPath { path }`.
`LocalProfileCatalog::list_profiles` concatenates, in this order, the embedded default (unless
disabled), embedded TOML profiles, the default user directory (`~/.qmt/profiles` via
`default_user_profiles_dir`, unless disabled or overridden), each extra `local_dir`, and
explicit files registered with `with_explicit_profile_file`. The result is sorted by id and
`ensure_unique_profile_ids` fails the whole listing on a duplicate id, and `load_profile`
refuses an id that matches more than one entry.

`list_local_profiles` reads only `*.toml` files directly inside a directory (no recursion). A
missing directory yields no profiles; a path that is not a directory is an error. The id is
`[profile].id` or else the file stem; the name defaults to the id.
`standard_embedded_profile_catalog_builder` provides the embedded `default` and
`coder-delegate` profiles with their prompt files inlined.

## Fingerprints

Local profiles carry a fingerprint, a 16-hex-digit hash of file content, size and
modification time (`profile_file_fingerprint`). Embedded profiles have none. The fingerprint is
only used to detect that a profile changed (reload) and is stored in a session's binding; it
is not a content-addressed identity.

## Provider modes and locks

`provider_mode` is `legacy` (default), `overlay` or `locked`. `providers` maps a logical
provider name to a requirement: an `oci://` artifact pinned to an `@sha256:` digest (64 hex
digits), a `factory`, a `kind` of `native` or `wasm`, and optional `plugin_api`, `features`
and `signer`. `validate_profile_provider_requirements` checks these at load time. In `locked`
mode it also requires every provider the config references (`agent.provider`, each
`execution.compaction.provider`, planner, each delegate, `quorum.delegation_summary`) to be
declared (`profile_provider_references`).

At materialization (`build_profile_runtime`), `legacy` uses the shared global plugin registry
unchanged. `overlay` and `locked` build a profile-specific registry
(`provision_profile_registry`): the declared providers, plus for `overlay` the global
providers the profile did not override. `resolved_profile_provider_lock` then records a
`sha256:` digest over the canonical JSON of the resolved providers.

## Materialization and the runtime manager

`ProfileRuntimeManager::runtime_for_profile` caches one `ProfileRuntime` per profile id.
Concurrent first requests share a per-id `OnceCell` in `building_runtimes`, and startup runs
outside the cache lock so unrelated cached profiles are never blocked. A single-agent config is
built with `Agent::builder_from_config_with_dotagents` (mesh settings from the config, the
protocol workspace threaded through); a quorum is built with
`Agent::builder_from_quorum_config(...).with_profile_id(id)`. Both run on the manager's shared
`AgentInfra`.

A runtime is only published if neither shutdown nor a reload happened while it was being
built. The manager's `generation` counter is read before the build and compared after; on a
mismatch the freshly built runtime is shut down and the build restarts.

## Reload

`reload_profiles` re-lists the catalog, bumps `generation`, moves the active profile to the
first listed profile (or `default`) if it vanished, and shuts down cached runtimes whose
fingerprint differs from the catalog's. Unchanged runtimes stay running. A failed re-list
leaves the cache untouched (`reload_profiles_logged` only warns). `start_profile_watcher`
watches the catalog's `watch_roots` and reloads on create, remove or non-access-time modify
events for `.toml` files.

## Session binding

A `SessionProfileBinding` records which profile (and, for delegated sessions, which `agent_id`)
owns a session, plus the profile fingerprint, source, config kind and provider lock. Resume and
routing code uses it to find the right runtime.

`session_binding` checks the in-memory map, then the persisted runtime binding in the session
store. It returns `None` (and warns) when the bound profile cannot be built, and also refuses
to resume when the stored provider lock digest differs from the current runtime's, so a session
is never silently continued on different provider artifacts.

`adopt_unbound_session` gives a session without a usable binding one: sessions with a
persisted binding to a profile that is still listed are left alone (a profile that merely
fails to load surfaces its error rather than being re-routed); otherwise the session is bound
to the embedded `default` profile if it is listed, else to the active profile.

## Invariants

- The `[profile]` table never reaches runtime config deserialization.
- Profile ids are unique across all catalog sources.
- A locked profile cannot reference an undeclared provider, and provider artifacts must be
  digest-pinned `oci://` references.
- A session is never resumed under a different provider lock than it was created with.
- A reload shuts down only runtimes whose fingerprint changed.

Source: `crates/agent/src/profiles.rs`, `crates/agent-runtime/src/profile_host.rs`,
`crates/agent-runtime/src/config/loader.rs`.
