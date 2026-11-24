# Spec Delta

## Purpose

Lets the user find and choose a model with the keyboard alone, wherever the model picker appears: the Start and session composers, the delegate dialog and setup.

## ADDED Requirements

### Requirement: Highlighted model row
The model picker SHALL highlight one selectable model row at a time, shown apart from the hover and the current-model marks. When the picker opens, the highlight SHALL be on the current model if it is listed, else on the first model. When the search text changes, the highlight SHALL move to the first model of the new results. Group headings and "more" lines SHALL never be highlighted.

#### Scenario: Picker opens on the current model
- **GIVEN** the session uses `claude-sonnet-4-5`
- **WHEN** the user opens the model picker
- **THEN** the `claude-sonnet-4-5` row is highlighted and scrolled into view

#### Scenario: Search moves the highlight to the best match
- **WHEN** the user types a search
- **THEN** the first model of the results is highlighted

### Requirement: Arrow keys move the highlight
While the picker's search field has focus, Down and Up SHALL move the highlight to the next and previous model, skipping group headings, and wrapping from the last model to the first and back. The highlighted row SHALL be scrolled into view. The arrow keys SHALL NOT move the text cursor of the search field.

#### Scenario: Down across a provider group
- **GIVEN** the highlight is on the last model of the "Anthropic" group
- **WHEN** the user presses Down
- **THEN** the first model of the next group is highlighted

#### Scenario: Wrap around
- **GIVEN** the highlight is on the last listed model
- **WHEN** the user presses Down
- **THEN** the first listed model is highlighted

#### Scenario: Highlight below the visible area
- **WHEN** the user presses Down until the highlight is on a row below the visible part of the list
- **THEN** the list scrolls so that row is visible

#### Scenario: Only listed models are reachable
- **GIVEN** a provider has more models than the list shows without a search
- **WHEN** the user moves the highlight through that group
- **THEN** only the listed rows are highlighted

### Requirement: Enter picks the highlighted model
Enter in the picker's search field SHALL choose the highlighted model exactly as clicking its row does. With no models listed, Enter SHALL do nothing.

#### Scenario: Choose with Enter
- **GIVEN** the user typed `sonnet` and pressed Down once
- **WHEN** the user presses Enter
- **THEN** the second result becomes the chosen model and the picker closes

#### Scenario: Nothing matches
- **GIVEN** the search matches no model
- **WHEN** the user presses Enter
- **THEN** nothing is chosen and the picker stays open

### Requirement: What a pick changes
Picking a model, by click or Enter, SHALL change the route of the delegate being edited while the delegate dialog is editing one. Otherwise it SHALL change the open session's model on the session page, and the model for new sessions on the Start page and in setup.

#### Scenario: Enter in the delegate dialog
- **GIVEN** the delegate dialog is editing delegate `coder` in an open session
- **WHEN** the user highlights a model and presses Enter
- **THEN** `coder` is routed to that model and the session's own model does not change

#### Scenario: Enter on the Start page
- **WHEN** the user picks a model with Enter on the Start page
- **THEN** it becomes the model for new sessions

### Requirement: Searching models
The model search SHALL match against each model's name, id, provider and serving node together, following the app's search rules (picker-search). Results stay grouped by provider: groups SHALL be ordered by their best match, and models within a group best first, so the best match is the first model listed. While searching, the "Recent" group SHALL be hidden and every match SHALL be listed.

#### Scenario: Words across fields
- **WHEN** the user searches `anthropic sonnet`
- **THEN** Anthropic's Sonnet models are listed

#### Scenario: Version written differently
- **WHEN** the user searches `sonnet 4.5` and a model's id is `claude-sonnet-4-5`
- **THEN** that model is listed

#### Scenario: Best match first
- **WHEN** the user searches `gpt5`
- **THEN** models whose name or id is `gpt-5…` are listed before models that only match loosely
