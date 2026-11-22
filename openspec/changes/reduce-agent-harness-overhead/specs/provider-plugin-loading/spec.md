# Spec Delta

## Purpose

Defines when the agent attempts to load a provider plugin again after an attempt failed, so that a plugin that cannot load is not retried on every model listing or status read.

## ADDED Requirements

### Requirement: A failed local plugin load is not repeated while its file is unchanged
After a provider plugin configured by a local path fails to load, the agent SHALL NOT attempt to load it again while the file at that path is in the same state as at the failed attempt: still missing, or the same size and modification time. This SHALL hold for model listings, status reads and requests that name the provider, except for an explicit `querymt/refreshModels` request.

#### Scenario: Missing plugin file and repeated model listings
- **WHEN** a configured provider's plugin file does not exist and a client lists models 100 times
- **THEN** the agent attempts to load that plugin once

#### Scenario: Missing plugin file and repeated status reads
- **WHEN** a configured provider's plugin file does not exist and a client reads authentication status 50 times
- **THEN** the agent attempts to load that plugin once

#### Scenario: Plugin file appears
- **WHEN** a plugin failed to load because its file was missing, the file is then created, and a client lists models
- **THEN** the agent attempts the load again and, when it succeeds, the provider's models are listed

#### Scenario: Plugin file is rebuilt
- **WHEN** a plugin failed to load because its file was invalid, the file is replaced with one of a different size or modification time, and a client lists models
- **THEN** the agent attempts the load again

#### Scenario: Replacement with the same size and modification time
- **WHEN** a plugin failed to load because its file was invalid and the file is replaced with one of the same size and modification time
- **THEN** a model listing does not attempt the load again, and a following `querymt/refreshModels` request does

### Requirement: A failed remote plugin fetch is retried after a wait
After a provider plugin fetched from a registry or URL fails to load, the agent SHALL NOT attempt to load it again until 300 seconds have passed since the failed attempt. This SHALL hold for model listings, status reads and requests that name the provider, except for an explicit `querymt/refreshModels` request.

#### Scenario: Remote plugin source within the wait
- **WHEN** fetching a plugin from a registry failed 60 seconds ago and a client lists models
- **THEN** the agent does not attempt the fetch again

#### Scenario: Remote plugin source after the wait
- **WHEN** fetching a plugin from a registry failed 300 seconds ago or earlier and a client lists models
- **THEN** the agent attempts the fetch again

### Requirement: A model refresh retries failed plugins
An explicit model refresh request (`querymt/refreshModels`) SHALL attempt to load every provider plugin whose last load attempt failed, whatever the state of its file and however recently it failed.

#### Scenario: Refresh retries a failed plugin
- **WHEN** a plugin failed to load 10 seconds ago, its file is unchanged, and a client requests a model refresh
- **THEN** the agent attempts to load that plugin again

#### Scenario: Refresh does not reload working plugins
- **WHEN** a client requests a model refresh and every configured plugin is already loaded
- **THEN** no plugin is loaded again

### Requirement: A remembered load failure is reported without a new attempt
While a failed load is not being repeated, a request that needs that provider SHALL fail with the error of the failed attempt, and other configured providers SHALL load and be listed as usual. The failure SHALL be logged as an error once per attempt, not once per request that is skipped.

#### Scenario: Session uses a provider whose plugin failed
- **WHEN** a session is configured with a provider whose plugin file is missing and the user sends a prompt
- **THEN** the run fails with an error naming the provider and the missing file path

#### Scenario: Other providers are unaffected
- **WHEN** one of four configured provider plugins fails to load and a client lists models
- **THEN** the models of the other three providers are listed

#### Scenario: Skipped attempts are not logged as errors
- **WHEN** a plugin failed to load once and a client then lists models 100 times with the file unchanged
- **THEN** the log contains one error for that plugin, not 101

### Requirement: Failed plugin load attempts are visible in traces
When telemetry is enabled, every plugin load attempt that fails SHALL be exported as a span with error status and the failure message. A request that skips a remembered failure SHALL NOT export a plugin load span.

#### Scenario: Failed attempt is an errored span
- **WHEN** a plugin load attempt fails because the file is missing
- **THEN** the exported load span for that provider has error status and a message naming the missing path

#### Scenario: Skipped attempt exports no span
- **WHEN** a model listing skips a plugin whose earlier load failed
- **THEN** no plugin load span is exported for that provider for that listing
