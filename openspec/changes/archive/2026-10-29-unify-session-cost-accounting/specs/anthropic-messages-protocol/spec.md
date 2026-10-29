# Spec Delta

## MODIFIED Requirements

### Requirement: Thinking tokens are reported as reasoning tokens
Usage SHALL report `output_tokens_details.thinking_tokens` as `reasoning_tokens` and SHALL subtract it from the provider's inclusive `output_tokens` to produce non-reasoning `output_tokens`, using saturating subtraction. Streaming usage SHALL end with authoritative final values, including downward corrections to provisional non-reasoning counts. When no thinking count is reported, `reasoning_tokens` SHALL be 0.

#### Scenario: Streaming response with thinking
- **WHEN** the final `message_delta` reports `output_tokens = 32000` and `thinking_tokens = 31950`
- **THEN** the response usage has `output_tokens = 50` and `reasoning_tokens = 31950`
- **AND** their sum remains the provider's inclusive 32000 output tokens

#### Scenario: Non-streaming response with thinking
- **WHEN** a non-streaming response reports `output_tokens = 600` and `thinking_tokens = 500`
- **THEN** the response usage has `output_tokens = 100` and `reasoning_tokens = 500`

#### Scenario: Provisional output exceeds final non-reasoning output
- **WHEN** streaming starts with inclusive output 1 and no thinking details, then finishes with inclusive output 100 and thinking 100
- **THEN** final response usage has `output_tokens = 0` and `reasoning_tokens = 100`
- **AND** the provisional ordinary output count does not survive as an extra token

#### Scenario: No thinking detail is available
- **WHEN** a response reports output tokens without `output_tokens_details.thinking_tokens`
- **THEN** usage retains the reported output count and reports zero reasoning tokens

## ADDED Requirements

### Requirement: Cache usage retains write-duration detail
Anthropic usage SHALL retain uncached `input_tokens`, cache reads, aggregate cache-creation tokens, and returned five-minute/one-hour cache-creation counts in streaming and non-streaming output. Write-duration counts SHALL be a breakdown of aggregate writes, not additional input tokens. Missing detail SHALL remain distinguishable from a returned zero count.

#### Scenario: Mixed cache-write durations
- **WHEN** usage reports 12 uncached input tokens, 30 cache reads, and 300 cache writes split into 200 five-minute and 100 one-hour tokens
- **THEN** canonical usage retains all counts and the total input count is 342
- **AND** both write-duration counts survive local output storage and remote provider transport

#### Scenario: Aggregate writes without duration detail
- **WHEN** usage reports 300 cache-creation tokens without a `cache_creation` breakdown
- **THEN** canonical usage retains 300 aggregate writes with unknown duration detail
- **AND** it does not invent 300 five-minute writes based on request configuration

#### Scenario: Streaming preserves start-event cache usage
- **WHEN** a start event reports uncached input, cache reads, and write-duration details, and a later output-usage event omits those input fields
- **THEN** final output retains the start event's cache information while using authoritative final output counts
- **AND** repeated events do not add duplicate cache tokens

#### Scenario: Cache-write detail disagrees with its aggregate
- **WHEN** the write-duration counts do not sum to reported aggregate writes
- **THEN** the available counts remain identifiable as inconsistent for accounting
- **AND** the provider path does not silently claim a complete cache-write estimate
