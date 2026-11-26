# Spec Delta

## ADDED Requirements

### Requirement: Picker rows show progress
In the list layout, a picker row for a session with progress SHALL fill its background from the left, as wide as the share of completed entries, and SHALL show the count as `done/total` before its status chip. A row for a session without progress SHALL look as it did before. Row text SHALL stay readable over the fill.

#### Scenario: Half-done session
- **GIVEN** session A has 2 of 4 todos done
- **WHEN** the user opens the session picker in the list layout
- **THEN** A's row is filled across half its width and shows `2/4`

#### Scenario: Session without todos
- **WHEN** the picker lists a session with no plan
- **THEN** its row has no fill and no count

#### Scenario: Finished plan
- **GIVEN** session A has 4 of 4 todos done
- **WHEN** the picker lists A
- **THEN** A's row is filled across its whole width in the success colour and shows `4/4`

#### Scenario: Progress moves while the picker is open
- **GIVEN** the picker is open and lists session A at 1 of 3
- **WHEN** the app learns that A is at 2 of 3
- **THEN** A's row shows `2/3` and its fill grows without the picker closing or reordering

### Requirement: Picker card grid
The picker SHALL offer a grid layout that shows the same sessions, in the same order, as cards in two columns. Each card SHALL show the title, the status chip, the agent, workspace and time since the last update, and, for a session with progress, a bar with `done/total`, the in-progress entry, the next two pending entries and the number of entries not shown. Clicking a card SHALL pick its session, as a row does.

#### Scenario: Card of a session with todos
- **GIVEN** session A has 6 todos: 2 completed, 1 in progress, 3 pending
- **WHEN** the picker shows the grid
- **THEN** A's card shows a bar at a third with `2/6`, the in-progress todo, the first two pending todos and "+1 more"

#### Scenario: Card of a session without todos
- **WHEN** the grid shows a session with no plan
- **THEN** its card shows the title, status and agent · workspace · time, and no bar or todo lines

#### Scenario: Same order in both layouts
- **GIVEN** one session waits for the user, one is running and six are idle
- **WHEN** the user switches from the list to the grid
- **THEN** the waiting session's card comes first, then the running one, then the idle ones newest first

#### Scenario: Pick from the grid
- **WHEN** the user clicks a card in the grid
- **THEN** the panel shows that session, as when its row is clicked in the list

### Requirement: Picker layout switch
The picker SHALL have a toggle next to its search input that switches between the list and the grid. The chosen layout SHALL be saved with the quick panel settings and used the next time the picker opens, including after a restart. The list SHALL be the default, and settings saved without a layout SHALL load with the list.

#### Scenario: Layout remembered
- **GIVEN** the user switched the picker to the grid and closed it
- **WHEN** the user opens the picker again after restarting the app
- **THEN** the picker opens in the grid

#### Scenario: Old settings
- **WHEN** the app loads settings saved before this change
- **THEN** the picker opens in the list layout

### Requirement: Grid picker fits the panel
While the picker is open in the grid layout, the panel SHALL be tall enough for all its cards up to the panel's height limit. When the cards need more room than that, the grid SHALL scroll inside the picker, with the search row and toggle staying in view.

#### Scenario: Grid in a compact panel
- **GIVEN** the panel is at its compact height on a display tall enough for the panel's full limit
- **WHEN** the user opens the picker in the grid layout with eight sessions
- **THEN** every card is visible without scrolling

#### Scenario: Grid on a small display
- **GIVEN** a display on which eight cards do not fit within the panel's limit
- **WHEN** the user opens the picker in the grid layout
- **THEN** the panel stops at its limit and the cards scroll under the search row
