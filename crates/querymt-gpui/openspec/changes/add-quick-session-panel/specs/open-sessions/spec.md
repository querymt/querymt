# Spec Delta

## Purpose
How the app keeps several sessions open at the same time, so that each surface (the main window, the quick panel) shows and drives its own session independently of the others.

## ADDED Requirements

### Requirement: Several sessions open at once
The app SHALL keep every session shown by a surface open at the same time. Each open session SHALL receive the agent inputs addressed to it (session updates, state updates, runtime and input-state notifications) while it is open, whether or not another session is open in another surface.

#### Scenario: Two sessions stream at the same time
- **GIVEN** session A is open in the main window and session B in the quick panel, both on the same agent
- **WHEN** both sessions are running and the agent streams replies for both
- **THEN** each surface shows its own session's reply as it streams, and neither transcript contains rows of the other session

#### Scenario: Update for a session that no surface shows
- **WHEN** an update arrives for a session that is not open in any surface
- **THEN** no transcript changes, and the session list still reflects the session's run state

### Requirement: Surfaces are independent
Opening, launching, reloading or navigating a session in one surface SHALL NOT change which session another surface shows, nor its scroll position, expanded rows, composer draft or attachments.

#### Scenario: Quick chat while the main window shows a session
- **GIVEN** session A is open in the main window with a draft in its composer
- **WHEN** the user starts a new session from the quick panel and sends a prompt
- **THEN** the main window still shows session A with its draft, and the new session appears in the quick panel only

#### Scenario: Navigating to a child session in one surface
- **GIVEN** the same session is open in the main window and the quick panel
- **WHEN** the user opens a delegate's child session in the main window
- **THEN** the quick panel still shows the parent session, and going back in the main window returns to the parent

### Requirement: One open session shared between surfaces
When two surfaces show the same session, the app SHALL hold it open once: the session is loaded once, and a prompt, steer, queued message, option change or undo made in one surface SHALL appear in the other as soon as it appears in the surface that made it.

#### Scenario: Prompt from the panel shows in the main window
- **GIVEN** session A is open in both the main window and the quick panel
- **WHEN** the user sends a prompt from the quick panel
- **THEN** the main window shows the pending prompt immediately, without reloading the session

#### Scenario: Opening a session that is already open
- **WHEN** a surface opens a session that another surface already shows
- **THEN** no second load request is sent and both surfaces show the same transcript

### Requirement: Open session lifetime
A session SHALL stay open while at least one surface shows it or keeps it to return to (a dismissed quick panel keeps its session until it releases it), and SHALL be closed when no surface holds it any more. A run started from a surface SHALL continue on the agent after the surface stops showing it, and its permission and input requests SHALL stay in the Inbox.

#### Scenario: Closing the quick panel during a run
- **GIVEN** a quick chat is running and is shown only in the quick panel
- **WHEN** the panel is dismissed
- **THEN** the run continues on the agent, the session list shows it running, and reopening it later shows the reply received meanwhile

#### Scenario: Quick panel releases its session
- **GIVEN** the quick panel was dismissed while showing session Q, and the main window does not show Q
- **WHEN** the panel's release time passes
- **THEN** session Q is closed in the app, stays listed in the sidebar, and opens again with its full conversation when picked

#### Scenario: Load completes after the surface closed
- **WHEN** a session's load response arrives after every surface that showed it has stopped showing it
- **THEN** the response is ignored and no other session changes

### Requirement: Launching does not choose the surface
Starting a new session SHALL show it only in the surface that started it. Starting a session from one surface SHALL NOT cancel or hide a session being started from another surface.

#### Scenario: Start page launch
- **WHEN** the user starts a session from the main window's start page
- **THEN** the main window shows the new session, as it does today

#### Scenario: Two launches at once
- **GIVEN** a session is being created from the main window's start page
- **WHEN** the user starts a quick chat from the quick panel before the first one finishes
- **THEN** both sessions are created, each surface shows its own, and each surface shows its own launch errors

### Requirement: Requests stay global
Permission and input requests SHALL remain listed in the Inbox for every session. A surface that shows a session SHALL also show that session's pending requests, and answering a request in any place SHALL resolve it everywhere.

#### Scenario: Answer a permission from the panel
- **GIVEN** a running session asks for a permission and is shown in the quick panel
- **WHEN** the user allows it in the quick panel
- **THEN** the request leaves the Inbox and the run continues
