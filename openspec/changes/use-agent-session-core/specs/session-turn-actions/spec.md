# Spec Delta

## Purpose
What undo, redo, fork, turn diff and turn copy act on in gpui's session view, now that a turn is one agent run that may contain several user messages.

## ADDED Requirements

### Requirement: Undo and redo by turn
Undo SHALL roll back the newest turn not already undone, targeting the first user message of that turn's run; the confirmation SHALL state how many turns it affects. Redo SHALL restore the turn on top of the agent's undo stack. Turns rolled back by the newest completed undo SHALL be shown as reverted. A turn without an agent-confirmed user message (pending prompt, run without a prompt) SHALL NOT be an undo target.

#### Scenario: Undo a steered turn
- **WHEN** the newest turn holds the prompt and a steering message, and the user undoes it
- **THEN** undo targets the turn's first prompt and the whole turn, including the steering message, is shown as reverted

#### Scenario: Pending prompt is not an undo target
- **WHEN** the newest turn is a pending prompt not yet echoed by the agent
- **THEN** undo targets the turn before it

#### Scenario: Redo restores the turn
- **WHEN** the user redoes after an undo
- **THEN** the turn on top of the undo stack is no longer shown as reverted

### Requirement: Fork from a turn
Forking from a turn SHALL branch after the turn's last agent-identified reply or reasoning, else after its first prompt, and SHALL carry the turn's first prompt and its reply text. A turn whose content has no agent ids yet SHALL NOT offer a fork.

#### Scenario: Fork after a reply
- **WHEN** the user forks from a completed turn
- **THEN** the fork branches after that turn's last reply and shows its prompt and reply

#### Scenario: Turn still streaming without ids
- **WHEN** a turn has only a pending prompt
- **THEN** the fork action is not offered for it

### Requirement: Turn diff and copy
The turn diff SHALL list every file changed by the tool calls of the whole turn, including tool calls after a steering message, grouped by file in first-touched order. Copying a turn SHALL copy all assistant text of the turn in order.

#### Scenario: Diff of a steered turn
- **WHEN** a turn edits one file before a steering message and another after it
- **THEN** the turn diff lists both files

#### Scenario: Copy a steered turn
- **WHEN** the user copies a turn with replies before and after a steering message
- **THEN** both replies are copied in order
