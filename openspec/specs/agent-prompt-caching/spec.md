# agent-prompt-caching Specification

## Purpose

Defines where the agent runtime places default prompt-cache hints on configured system sections and recent conversation messages, so stable prefixes are reused within provider breakpoint limits.

## Requirements

### Requirement: Default system cache hints mark only the first and last sections
For Anthropic, Codex and OpenAI Responses sessions, the agent SHALL add a default cache hint (provider-default lifetime) only to the first and the last system section of the resolved system prompt. Sections between them SHALL receive no default hint. A single section SHALL receive one hint. Other providers SHALL receive no default system hints.

#### Scenario: Four sections get two default hints
- **WHEN** an Anthropic session's system prompt has four sections with no cache settings
- **THEN** sections 1 and 4 carry a default cache hint
- **AND** sections 2 and 3 carry no cache hint

#### Scenario: Single section gets one default hint
- **WHEN** an Anthropic session's system prompt is a single plain string
- **THEN** that section carries one default cache hint

#### Scenario: Dotagents sections count as system sections
- **WHEN** an agent with one `system` file runs in a project with dotagents `SYSTEM.md` and `AGENTS.md`
- **THEN** only the agent's file section and the `AGENTS.md` section carry default cache hints

#### Scenario: Providers without cache defaults are untouched
- **WHEN** a session uses OpenAI Chat Completions with three system sections
- **THEN** no system section carries a cache hint

### Requirement: Explicit system cache settings are preserved
A system section that already has a `cache` hint (including an explicit `null`) or a native `cache_control` SHALL be left unchanged, at any position. Default hints SHALL still be added to the first and last sections when they have no explicit setting.

#### Scenario: Explicit middle hint is kept
- **WHEN** a system prompt has three sections and section 2 is configured with `cache = { ephemeral = { ttl_seconds = 1800 } }`
- **THEN** section 2 keeps that hint
- **AND** sections 1 and 3 carry default hints

#### Scenario: Explicit null opts the last section out
- **WHEN** the last of two system sections is configured with `cache = null`
- **THEN** only section 1 carries a cache hint

### Requirement: The two most recent messages carry cache hints
Before each model request, the agent SHALL add a default cache hint to each of the last two messages that has none. Hints already present on any message SHALL be kept. Applying this twice SHALL yield the same request as applying it once.

#### Scenario: Last two messages are hinted
- **WHEN** the agent sends a request with five messages and no existing hints
- **THEN** only messages 4 and 5 carry a default cache hint

#### Scenario: Existing history hints are kept
- **WHEN** message 1 of five already carries a cache hint
- **THEN** messages 1, 4 and 5 carry cache hints in the request
