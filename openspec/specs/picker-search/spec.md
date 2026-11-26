# picker-search Specification

## Purpose

Defines how the app's search boxes match and rank items, so the model picker, the profile and target selects, the workspace field and the slash command lists all find things the same way.

## Requirements

### Requirement: Fuzzy matching across an item's fields
An item SHALL match a search when every whitespace-separated word of the search matches the item. A word matches when its characters appear in order, with gaps allowed, in at least one of the item's searchable fields. Matching SHALL ignore case. An empty search SHALL match every item in its original order.

#### Scenario: Letters in order with gaps
- **GIVEN** a profile named "Coder Delegate"
- **WHEN** the user searches `cdl`
- **THEN** the profile is listed

#### Scenario: Words in different fields
- **GIVEN** a host named "thor" whose peer id contains `9f3e`
- **WHEN** the user searches `thor 9f3e`
- **THEN** the host is listed

#### Scenario: One word does not match
- **WHEN** the user searches `coder zzz`
- **THEN** "Coder Delegate" is not listed

### Requirement: Separators do not block a match
Punctuation that separates parts of names and versions (`-`, `_`, `.`, `/`, `:`) SHALL NOT keep an otherwise matching search from matching, whether it appears in the search or in the item.

#### Scenario: Dots against hyphens
- **WHEN** the user searches `sonnet 4.5` and an item's id is `claude-sonnet-4-5`
- **THEN** the item is listed

#### Scenario: Missing hyphen
- **WHEN** the user searches `gpt5` and an item's id is `gpt-5`
- **THEN** the item is listed

### Requirement: Small typos are tolerated
A search word of five or more characters SHALL still match when about one character in four is missing or wrong. Shorter words SHALL match exactly in order.

#### Scenario: Missing letter
- **WHEN** the user searches `sonet`
- **THEN** items named "Sonnet" are listed

#### Scenario: Short word stays strict
- **WHEN** the user searches `gpz`
- **THEN** items that only contain `gpt` are not listed

### Requirement: Best match first
Results SHALL be ordered by match quality. Contiguous matches and matches at the start of a word rank above scattered ones. Items that match equally well SHALL keep their original order.

#### Scenario: Contiguous before scattered
- **GIVEN** profiles `def-qmcp` and "Default (Speech)"
- **WHEN** the user searches `qmcp`
- **THEN** `def-qmcp` is listed first

#### Scenario: Equal matches keep list order
- **GIVEN** hosts "thor-a" and "thor-b" listed in that order
- **WHEN** the user searches `thor`
- **THEN** "thor-a" is listed before "thor-b"

### Requirement: An item's name ranks above its description
A match in an item's own name SHALL rank above the same match only in a field that describes or locates the item: a model's provider, id or node; a profile's id or description; a workspace's path; a palette entry's keywords. Such a field SHALL still let an item match, and an item that matches in both SHALL be ranked by the better of the two.

#### Scenario: Name before provider
- **GIVEN** the models `inkling-2-mini` of the provider `openrouter` and `claude-opus-4` of the provider `anthropic`
- **WHEN** the user searches `op`
- **THEN** `claude-opus-4` is listed before `inkling-2-mini`, which is still listed because its provider begins with `op`

#### Scenario: Name before provider within a provider
- **GIVEN** the models `inkling-2-mini` and `claude-opus-4`, both of the provider `openrouter`
- **WHEN** the user searches `op`
- **THEN** `claude-opus-4` is listed first

#### Scenario: A description alone still matches
- **GIVEN** the model `inkling-2-mini` of the provider `openrouter`
- **WHEN** the user searches `openrouter`
- **THEN** `inkling-2-mini` is listed

### Requirement: Search text is literal
Characters such as `!`, `^`, `'` and `$` in a search SHALL be matched as ordinary characters, never as search operators.

#### Scenario: Caret in a search
- **WHEN** the user searches `^foo`
- **THEN** items are matched as if `^` were an ordinary character, not only items that start with `foo`
