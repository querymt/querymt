# Spec Delta

## ADDED Requirements

### Requirement: Picker rows say where a session runs
In the list layout, a picker row SHALL show the session's title over a subtitle with the last two parts of its folder. For a worktree session the folder SHALL be the project it was made from, with a branch mark, and the branch SHALL be named only when it is not a generated `qmt/workspace/<id>` one. The agent's name SHALL be shown only when the list holds sessions of more than one agent.

#### Scenario: Worktree session
- **GIVEN** session A runs in a worktree made from `~/qmt/querymt` on branch `qmt/workspace/<id>`, and session B in one on branch `fix-login`
- **WHEN** the picker lists A and B
- **THEN** both subtitles read `qmt/querymt` with a branch mark, and only B's names its branch

### Requirement: Picker rows show progress
After its text, a list row SHALL show a bar with one section per todo, in a colour that says whether the session is live:
- running: the accent, completed sections solid, the section in progress breathing, pending ones faint;
- running with no todo in progress: the whole bar breathing;
- running without todos: a segment sliding along the track;
- waiting for the user: the warning colour, still;
- idle: the todos as they were left, in a dim grey, still; no bar without todos.

#### Scenario: Live session
- **GIVEN** session A is running with 4 todos: 2 completed, 1 in progress, 1 pending
- **WHEN** the user opens the session picker in the list layout
- **THEN** A's row shows a bar of four sections after its title, the third breathing

#### Scenario: Waiting session
- **GIVEN** session A has todos and waits for the user
- **WHEN** the picker lists A
- **THEN** A's bar is in the warning colour and still

#### Scenario: Running session without todos
- **WHEN** the picker lists a running session with no plan
- **THEN** its bar is a segment sliding along the track

#### Scenario: Idle session with todos
- **GIVEN** session A is idle with 3 todos, 1 completed
- **WHEN** the picker lists A
- **THEN** A's bar shows three still grey sections, the first stronger

#### Scenario: Session without todos
- **WHEN** the picker lists an idle session with no plan
- **THEN** its row has no bar, and its title is in the column of the other rows' titles

#### Scenario: Progress moves while the picker is open
- **GIVEN** the picker is open and lists session A at 1 of 3, working on the second todo
- **WHEN** the app learns that A is at 2 of 3, working on the third
- **THEN** A's bar has one more section done and its subtitle names the third todo, without the picker closing or reordering

### Requirement: The bar stays readable and calm
Above twenty todos the bar SHALL be drawn as one track filled to the share completed. With reduced motion or in Low Power Mode nothing in the bar SHALL move: the section in progress stays at a fixed strength and the sliding segment rests mid-track. The panel SHALL request frames for the bar only while some row is moving.

#### Scenario: Reduced motion
- **GIVEN** the system asks for reduced motion
- **WHEN** the picker lists a running session
- **THEN** its bar is drawn still, with the section in progress set apart by strength alone

### Requirement: Picker rows say what a session is doing or when it ran
A live (running or waiting) session's subtitle SHALL end with the todo in progress, when there is one, and its row SHALL show no time. An idle row SHALL show how long ago the session was active and no todo. Only a waiting session SHALL show a status chip, "Needs input". A row SHALL NOT show a `done/total` count, a "Running" chip or a spinner, and its background SHALL NOT be tinted by progress.

#### Scenario: What a live session is doing
- **GIVEN** session A is running with a todo in progress, "todo 2"
- **WHEN** the picker lists A
- **THEN** A's subtitle ends with "todo 2" and its row shows no time

#### Scenario: When an idle session ran
- **GIVEN** session A is idle with todos and was last active on 5 January
- **WHEN** the picker lists A
- **THEN** A's row shows "Jan 5" and its subtitle names no todo

#### Scenario: Waiting session asks for the user
- **GIVEN** session A waits for the user
- **WHEN** the picker lists A
- **THEN** A's row shows "Needs input"

### Requirement: Rows keep their columns
While some row of the list has a bar, rows without one SHALL keep the bar's place empty, so bars and times form columns and titles have the same width; a list where no session has a bar SHALL keep no place for one. Progress SHALL NOT make a row taller.

#### Scenario: No session has a bar
- **WHEN** the picker lists only idle sessions without todos
- **THEN** no place is kept for a bar and the titles take its width

### Requirement: One highlight in the picker
The picker, in both layouts, SHALL show one highlighted row or card: the one the keyboard is on. Moving the pointer over a row or card SHALL move the keyboard's position to it, so a resting pointer never shows a second highlight. Rows and cards SHALL have the same corner radius, concentric with the panel's corner and the list's inset.

#### Scenario: Pointer and keyboard
- **GIVEN** the picker is open and the keyboard is on the first row
- **WHEN** the pointer moves over the second row
- **THEN** the second row is the highlighted one, and the first is not

### Requirement: Picker card grid
The picker SHALL offer a grid layout that shows the same sessions, in the same order, as cards in two columns. Each card SHALL show the title, the status chip, the agent, workspace and time since the last update, and, for a session with a bar in the list, the same bar across the card, the in-progress entry, the next two pending entries and the number of entries not shown. A card SHALL be as tall as its content. Clicking a card SHALL pick its session, as a row does.

#### Scenario: Card of a session with todos
- **GIVEN** session A is running with 6 todos: 2 completed, 1 in progress, 3 pending
- **WHEN** the picker shows the grid
- **THEN** A's card shows a bar of six sections across the card, the in-progress todo, the first two pending todos and "+1 more"

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
