# Spec Delta

## Purpose

Defines how the host loads native provider plugins (shared libraries) and calls them through a versioned interface. Only serialized data and C function pointers cross that interface, so a plugin keeps working with any host of the same ABI version, whichever compiler built either side.

## ADDED Requirements

### Requirement: Native plugins are accepted only for the host's exact ABI version
Before calling anything else in a native plugin library, the host SHALL read the ABI version the library reports. It SHALL load the library only when that version equals the host's native plugin ABI version (3 for this change). There is no compatibility window: every other version, and every library that reports no version, SHALL be rejected with an error naming the library path, the reported version (or its absence) and the host's version.

#### Scenario: Plugin built for the host ABI loads
- **WHEN** a provider entry points at a native library that reports ABI version 3 and the host's ABI version is 3
- **THEN** the provider loads and its factory name, config schema and models are available.

#### Scenario: Plugin reports a different ABI version
- **WHEN** a native library reports ABI version 4 and the host's ABI version is 3
- **THEN** loading that provider fails with an error naming the library path, "4" and "3"
- **AND** no factory or provider function of the library is called.

#### Scenario: Library built for the previous native interface is rejected
- **WHEN** a native library exports only the previous interface (`querymt_plugin_api`, `plugin_factory` or `plugin_http_factory`) and no ABI version for the new interface
- **THEN** loading fails with an error saying the library is not a QueryMT native plugin for ABI 3
- **AND** the error tells the user to install a plugin built for this host or, for HTTP providers, to use the Wasm plugin.

### Requirement: Configured native ABI must match the host
The optional key `plugin_api` (unsigned integer, no default) in a providers config entry (`[[providers]] plugin_api`) and in a profile provider requirement (`[profile.providers.<name>] plugin_api`) states the native ABI version the entry requires. When it is absent, the host's own version is required. A value other than the host's version SHALL make that provider fail to load. A value that is not an unsigned integer SHALL be a config load error.

#### Scenario: Configured ABI equals the host ABI
- **WHEN** a provider entry sets `plugin_api = 3`, the host's ABI version is 3 and the library reports 3
- **THEN** the provider loads.

#### Scenario: Configured ABI differs from the host ABI
- **WHEN** a provider entry sets `plugin_api = 2` and the host's ABI version is 3
- **THEN** that provider fails to load with an error naming the configured value 2 and the host version 3
- **AND** other providers in the same config still load.

#### Scenario: Configured ABI is not an unsigned integer
- **WHEN** a provider entry sets `plugin_api = -1` or `plugin_api = "3"`
- **THEN** loading the providers config fails with an error naming the `plugin_api` key.

### Requirement: Native plugins work regardless of compiler version
A native plugin built for the host's ABI version SHALL load and behave identically whichever Rust compiler release built the host and the plugin. Host and plugin SHALL share no Rust type layout, memory allocator, standard library instance or async runtime state.

#### Scenario: Plugin built with a different compiler release
- **WHEN** a host built with one Rust compiler release loads a plugin built from the same source with a different release
- **THEN** the plugin loads, and chat, streaming, error and log behavior match the same plugin built with the host's compiler.

### Requirement: Factory operations are available through the native interface
The host SHALL expose the plugin's factory name, JSON config schema, custom-model support flag and model listing, and SHALL create provider instances from a JSON config. Errors from these operations SHALL reach the caller as the error the plugin returned. For locked provider entries the factory name SHALL equal the expected factory or loading fails.

#### Scenario: Provider is created from a valid config
- **WHEN** the host creates a provider from a config that the plugin accepts
- **THEN** the provider instance is usable for every operation the plugin implements.

#### Scenario: Plugin rejects a provider config
- **WHEN** the host creates a provider from a config the plugin rejects
- **THEN** creation fails with the plugin's error kind and message.

#### Scenario: Locked entry with a different factory name
- **WHEN** a locked provider entry expects factory `llama_cpp` and the plugin reports `mrs`
- **THEN** loading fails with an error naming both factory names.

### Requirement: Provider operations round-trip losslessly
The host SHALL forward chat, streamed chat, completion, embedding, transcription, speech and provider tool calls to the plugin, and SHALL report the plugin's tools and chat capabilities. Requests and results SHALL arrive unchanged, including structured output items, binary attachments and audio. An operation the plugin does not implement SHALL fail with a not-implemented error.

#### Scenario: Chat result matches the in-process provider
- **WHEN** the same chat request is sent to a provider through the native interface and to the same provider linked into the host
- **THEN** both return equal structured output, usage and finish reason.

#### Scenario: Binary audio crosses the interface unchanged
- **WHEN** a transcription request carries audio bytes and a speech response carries audio bytes
- **THEN** the plugin receives exactly the request bytes and the host receives exactly the response bytes.

#### Scenario: Operation not implemented by the plugin
- **WHEN** the host calls an operation, such as embedding, that the plugin's provider does not implement
- **THEN** the call fails with a not-implemented error and the provider remains usable.

### Requirement: Streams are ordered and cancellable
Streamed chat SHALL deliver the plugin's chunks in production order and end after the plugin's terminal chunk or error. When the host drops a stream or an in-flight call, the host SHALL signal the plugin to stop, and the plugin's call SHALL observe cancellation at its next produced chunk. Waiting for plugin results SHALL NOT block host async executor threads.

#### Scenario: Stream delivers chunks in order
- **WHEN** a plugin streams chunks c1..cN followed by a terminal chunk
- **THEN** the host stream yields c1..cN and the terminal chunk in that order and then ends.

#### Scenario: Dropping a stream stops plugin generation
- **WHEN** the host drops a chat stream while the plugin is still producing chunks
- **THEN** the plugin's next attempt to deliver a chunk reports cancellation and the plugin stops producing chunks for that call.

#### Scenario: Dropping an in-flight call cancels it
- **WHEN** the host drops a pending non-streaming call
- **THEN** the plugin's call is cancelled and its result, if any, is discarded without error.

#### Scenario: Slow plugin call does not block the host executor
- **WHEN** a plugin call takes several seconds on a host whose async runtime has one worker thread
- **THEN** other tasks on that runtime keep making progress during the call.

### Requirement: Plugin errors keep their details
Errors returned by plugin operations SHALL reach the host with the same error kind and the details that kind carries, including message, HTTP status, provider failure kind and code, and retry-after delay.

#### Scenario: Rate-limit error keeps its retry delay
- **WHEN** a plugin call fails with a rate-limited error carrying a 30-second retry delay
- **THEN** the host receives a rate-limited error with a 30-second retry delay.

### Requirement: Plugin panics are contained
A panic in plugin code during any factory, provider or stream operation SHALL fail only that operation with a plugin error that contains the panic message. It SHALL NOT unwind into the host or abort the host process, and the provider SHALL remain usable for later calls.

#### Scenario: Panicking chat call
- **WHEN** a provider's chat implementation panics with message "boom"
- **THEN** the host's chat call fails with a plugin error containing "boom"
- **AND** a following chat call to the same provider is served normally.

#### Scenario: Panic while streaming
- **WHEN** a provider panics after producing two stream chunks
- **THEN** the host stream yields the two chunks, then a plugin error containing the panic message, and then ends.

### Requirement: Plugin code has its own async runtime
Provider operations SHALL run on an async runtime owned by the plugin. Provider code that spawns tasks or needs the current runtime SHALL work whichever host thread, or non-runtime thread, issued the call.

#### Scenario: Provider spawns async tasks
- **WHEN** a provider operation spawns a task on the current async runtime and awaits its result
- **THEN** the operation completes successfully when called from a host async task and when called from a plain host thread.

### Requirement: Plugin log records reach the host logger
Log records emitted by plugin code SHALL be forwarded to the host logger with their level, target and message. The host's maximum log level, as set when the plugin is loaded, SHALL limit which records the plugin forwards.

#### Scenario: Plugin warning appears in host logs
- **WHEN** plugin code logs a warning with target `llama_cpp::model` and the host's maximum level is info
- **THEN** the host logger receives a warning record with target `llama_cpp::model` and the same message.

#### Scenario: Records above the host level are not forwarded
- **WHEN** plugin code logs a trace record and the host's maximum level is info
- **THEN** the host logger receives no record for it.

### Requirement: Native plugin libraries are never unloaded
A loaded native plugin library SHALL stay mapped until the host process exits, so that providers, in-flight calls and streams created from it never outlive its code. Loading the same library file again in the same process SHALL reuse the loaded library and SHALL NOT initialize it again.

#### Scenario: Provider outlives its factory
- **WHEN** a provider created from a native plugin is still in use after the registry entry that loaded the plugin is replaced
- **THEN** calls and streams on that provider keep working.

#### Scenario: Same library configured twice
- **WHEN** two provider entries point at the same native library file
- **THEN** both providers load and the library's one-time initialization runs once.
