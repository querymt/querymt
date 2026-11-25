# Spec Delta

## Purpose
A floating conversation panel summoned by a global shortcut on desktop, for quick chats and for reacting to any session without bringing up the main window or changing what it shows.

## ADDED Requirements

### Requirement: Summon and dismiss
On desktop builds the app SHALL toggle the quick panel when its global shortcut is pressed, from any application. The panel SHALL appear above other windows with keyboard focus in its composer, where the user left it on that display, else centered on the main window's display or the primary one; where the desktop places such panels itself (Wayland), it SHALL appear where the desktop puts it. It SHALL close when the user presses Esc, presses the shortcut again, or focus moves to another window.

#### Scenario: Summon over another application
- **GIVEN** another application is focused
- **WHEN** the user presses the quick panel shortcut
- **THEN** the panel appears above that application with the composer focused, and the main window does not come to the front

#### Scenario: Dismiss by clicking elsewhere
- **WHEN** the panel is open and the user clicks another window
- **THEN** the panel closes

#### Scenario: Esc inside the composer
- **WHEN** the panel is open, the composer has text and the user presses Esc
- **THEN** the panel closes and the draft is kept for the next time the panel opens

### Requirement: Looks like a HUD
The panel SHALL be dark whatever the app's theme or the desktop's, with a look of its own that is visibly different from the main window: rounded corners, no rule under its header, a floating composer, assistant text without a card, user messages as a tinted pill, and tool calls as one-line chips. All of it, markdown and diffs included, SHALL use the panel's own colours.

#### Scenario: Light app theme
- **GIVEN** the app theme is light and the desktop is set to light
- **WHEN** the panel is summoned
- **THEN** its material, surfaces and text are dark, and the main window is unchanged

#### Scenario: Transcript uses the panel's colours
- **GIVEN** the panel shows a session with markdown, a code block and a diff
- **THEN** all of them use the panel's colours, not the main window's

#### Scenario: Overlays sit on the panel
- **WHEN** the session list, the model picker, the folder chooser or the composer's command list is open
- **THEN** it is drawn on the panel's own surface, with no separately tinted box, no dimming of the panel and no shadow over the material

#### Scenario: Typed text in a light app theme
- **GIVEN** the app theme is light
- **WHEN** the user types in the panel's composer or in a search field of the panel
- **THEN** the text, its placeholder, its selection and its caret use the panel's colours and are light on the dark panel

### Requirement: A new quick chat is one bar
Before a conversation is shown, the panel SHALL be a single bar holding the input, what the chat will run on and a way to pick a session, with no header. The header, with its new-chat, session-picker and open-in-main-window actions, SHALL appear with a conversation.

#### Scenario: New quick chat
- **GIVEN** the panel is open and shows no session
- **THEN** it is one bar with the input, the agent, model and directory, and a button for the session picker, and it has no header

#### Scenario: Conversation shown
- **WHEN** a quick chat starts or a session is picked
- **THEN** the panel grows around the conversation and shows its header

### Requirement: Uses its desktop's material
The panel SHALL use the translucent material its desktop offers, drawn dark where the desktop allows it and in its active state even though the panel never becomes the desktop's active window. The material SHALL be visible through the panel's surfaces. Where it can be light, the panel SHALL tint it dark enough for text to stay legible; where there is none, the panel SHALL be near-opaque dark. The panel is not required to look the same on every desktop.

#### Scenario: Light backdrop chosen by the system
- **GIVEN** a desktop whose material is light and cannot be set to dark
- **WHEN** the panel is summoned
- **THEN** its text keeps readable contrast over the material

#### Scenario: Desktop without a translucent material
- **GIVEN** a desktop that offers no blur for application windows
- **WHEN** the panel is summoned
- **THEN** it shows the same layout on an opaque surface, with no transparent or unreadable areas

#### Scenario: Bright window behind the panel
- **GIVEN** a white window lies behind the panel
- **WHEN** the panel is summoned
- **THEN** the conversation text, the composer text, and the model, folder, mode and title labels keep a contrast of at least 4.5:1 against what is drawn behind them

#### Scenario: Reduce transparency
- **GIVEN** the system asks to reduce transparency
- **WHEN** the panel is summoned
- **THEN** it is drawn on an opaque dark surface, as on a desktop without a material

#### Scenario: Panel never becomes the active window
- **WHEN** the panel is open above another application that stays active
- **THEN** its material is drawn as active, not greyed out

### Requirement: Opens with motion
Summoning the panel SHALL fade and scale it in with easing, material included, and SHALL end with a small bump as it reaches its size, without ever drawing larger than its final size. The composer SHALL accept keystrokes from the first frame of that animation, and dismissal SHALL be immediate. When the system asks for reduced motion, the panel SHALL appear in its final state without the animation.

#### Scenario: Typing during the opening animation
- **WHEN** the user presses the shortcut and types immediately
- **THEN** every character typed reaches the composer

#### Scenario: Reduced motion
- **GIVEN** the system is set to reduce motion
- **WHEN** the panel is summoned
- **THEN** it appears at its final size and opacity without animating

### Requirement: Returns to the last conversation
Reopening the panel SHALL show the session it showed when it was dismissed, with its draft, unless the user started a new quick chat. After the panel has stayed closed for 15 minutes, the next opening SHALL start on a new, empty quick chat.

#### Scenario: Reopen shortly after
- **GIVEN** the panel showed session Q and was dismissed one minute ago
- **WHEN** the user summons the panel
- **THEN** it shows session Q at its latest reply

### Requirement: Quick chat
The panel SHALL let the user start a new session by typing a prompt and pressing Enter. A quick chat SHALL run on the primary agent with the launch defaults of new sessions (model, mode, reasoning, profile, working directory), and the panel SHALL show which model and directory it uses (and which agent, when several are configured) before the first prompt. The user SHALL be able to change the directory, the model and the reasoning effort of the next quick chat in the panel, by keyboard or mouse, without changing the start page's launch defaults. A quick chat SHALL be an ordinary session, listed in the sidebar like any other.

#### Scenario: Start a quick chat
- **GIVEN** the panel is open on an empty quick chat and the primary agent is connected
- **WHEN** the user types a question and presses Enter
- **THEN** a new session is created, the prompt shows as pending at once, and the reply streams into the panel

#### Scenario: Primary agent not connected
- **WHEN** the user submits a quick chat while the primary agent is not connected
- **THEN** the prompt stays in the composer and the panel says the agent is not connected

#### Scenario: Choose the folder
- **GIVEN** the panel is open on an empty quick chat
- **WHEN** the user presses Cmd/Ctrl+O or clicks the folder, and picks or types a folder
- **THEN** the next quick chat starts in that folder, Esc closes the chooser first and not the panel, and a quick chat with no folder known opens the chooser instead of failing

#### Scenario: Start in the main window
- **WHEN** the user presses Cmd/Ctrl+Enter in the bar of an empty quick chat
- **THEN** the quick chat starts and its session is shown in the main window

#### Scenario: Reasoning effort by keyboard
- **WHEN** the user presses Cmd/Ctrl+T in the panel
- **THEN** the next quick chat, or the session shown, moves to the next reasoning effort

### Requirement: Pick a session
The panel SHALL let the user switch to any existing session by searching sessions by title, workspace or agent, with sessions that wait for the user or are running listed first. Switching SHALL change only the panel. In the list, Up and Down SHALL move over the rows, Enter SHALL show the marked row, and Cmd/Ctrl+1 to 8 SHALL show that row, which carries its shortcut as a hint. The model picker SHALL be navigable the same way (Up, Down, Enter).

#### Scenario: React to a running session
- **GIVEN** session A is running in the background and is not shown in the main window
- **WHEN** the user picks session A in the panel
- **THEN** the panel shows session A's conversation with its live reply, and the main window is unchanged

### Requirement: Session list in place of the conversation
The session list, the model picker and the folder chooser SHALL each open in place of the conversation, inside the panel's own surface rather than as a separate card over it, and the conversation SHALL return when they close. The folder chooser's suggestions SHALL be part of it, not a dropdown over the panel.

#### Scenario: List in place of the conversation
- **GIVEN** the panel shows a conversation
- **WHEN** the user opens the session list, the model picker or the folder chooser
- **THEN** it takes the conversation's place under the header, and closing it shows the conversation again at the same scroll position

#### Scenario: Every folder suggestion is visible
- **GIVEN** the folder chooser offers suggestions
- **WHEN** it is open
- **THEN** every suggestion is inside the panel, or, once they need more room than the panel's limit, they scroll

### Requirement: Move the panel
The user SHALL be able to move the panel by dragging its header, or the empty parts of a new quick chat's bar, with the pointer; dragging SHALL NOT start from the text field or a button. The panel SHALL reopen where it was last left on that display, also after the app restarts, kept inside the display's visible area. Where the desktop places the panel itself (Wayland layer-shell), it cannot be moved.

#### Scenario: Drag the panel
- **WHEN** the user drags the panel's header
- **THEN** the panel follows the pointer, and keeps growing from where it was left

#### Scenario: Reopen where it was left
- **GIVEN** the user moved the panel and closed it, or quit the app
- **WHEN** the panel is summoned on the same display
- **THEN** it appears where it was left, moved inside the visible area if the display changed size

#### Scenario: Typing is not dragging
- **WHEN** the user presses in the composer and moves the pointer
- **THEN** text is selected and the panel stays put

### Requirement: Compact conversation
The panel SHALL show the picked session's conversation in a compact form of the main transcript: the same messages, tool calls, reasoning, compactions and run state, with tool calls and reasoning collapsed to one line each until expanded. Its composer SHALL be one line at rest, so that its placeholder and its buttons share a centre line, and SHALL grow with the text, and SHALL grow with the text up to a limit. The panel SHALL send a prompt, steer or queue a message while a run is active, and stop the run, under the same rules as the main window.

#### Scenario: Tool call in the panel
- **WHEN** a reply in the panel contains a tool call
- **THEN** it shows as one line with its name, target and status, and expands inline when clicked

#### Scenario: Message during a run
- **GIVEN** the session shown in the panel is running
- **WHEN** the user sends a message from the panel
- **THEN** it is steered into the run, or queued when the run cannot be steered, as in the main window

### Requirement: Grows with the conversation
The panel SHALL open at a compact height and SHALL grow as its conversation grows, up to a limit set by the display it is on, after which the conversation SHALL scroll. While a conversation is shown the panel SHALL NOT shrink on its own. Starting a new quick chat SHALL return it to its compact height. The panel SHALL always be tall enough for whatever it shows over the conversation, including its session picker, its model picker and its errors.

#### Scenario: First replies arrive
- **GIVEN** the panel is open at its compact height on a new quick chat
- **WHEN** the agent's first reply streams in
- **THEN** the panel grows to show it, and the composer stays where the user is typing

#### Scenario: Long conversation
- **GIVEN** a session whose conversation is taller than the panel's limit
- **WHEN** the panel shows it
- **THEN** the panel stops at its limit and the conversation scrolls inside it

#### Scenario: Picker in a small panel
- **GIVEN** the panel is at its compact height
- **WHEN** the user opens the session picker or the model picker
- **THEN** the whole picker is visible inside the panel

#### Scenario: Every listed session is visible
- **GIVEN** the session list holds as many sessions as it shows, at any text size the app offers
- **WHEN** the list is open
- **THEN** every row is fully inside the panel, or, once the list needs more room than the panel's limit, the rows scroll under the search row

#### Scenario: Back to a new quick chat
- **GIVEN** the panel has grown around a long conversation
- **WHEN** the user starts a new quick chat
- **THEN** the panel returns to its compact height

### Requirement: Shows that the agent is working
While the session shown in the panel is streaming a reply or running a tool, the panel SHALL show a moving light along its edge, and SHALL stop it when the run ends or the panel shows an idle session. The light SHALL stay at the edge, clear of the conversation's text. When the system asks for reduced motion, the light SHALL be still rather than moving. An idle panel SHALL cost no animation frames.

#### Scenario: Reply streaming
- **WHEN** the agent starts streaming a reply into the panel
- **THEN** the panel's edge shows it is working, and goes back to its resting look when the run ends

#### Scenario: Idle session
- **GIVEN** the panel shows a session with no run in progress
- **THEN** nothing on its edge moves, and no animation frames are requested

### Requirement: The edge follows the run
On every desktop the light along the edge SHALL follow what the agent is doing, with the same states, and its end-of-turn and error signals SHALL only follow a run that ends while its session is shown. On macOS it SHALL run without work on the app's main thread between changes; where it cannot, the panel SHALL draw the same states itself.

#### Scenario: The edge follows the run
- **GIVEN** the light is running
- **WHEN** text streams faster, a tool call starts, the agent waits for the user, an error occurs, or the turn ends
- **THEN** the light travels faster, flares once, stops travelling and breathes slowly in another colour, pulses red once, or finishes its lap and fades, respectively

#### Scenario: A run too short to notice
- **WHEN** a run starts and ends within a fraction of a second
- **THEN** the edge shows nothing

#### Scenario: A short run ends
- **WHEN** a run of under a second ends cleanly
- **THEN** the light fades out without the finishing lap

#### Scenario: The macOS light cannot run
- **GIVEN** on macOS the light cannot be created, fails while running, or the panel has no material of its own
- **THEN** the panel draws the same states itself and works as before

#### Scenario: The edge and resizing
- **WHEN** the panel grows with the conversation while the light runs
- **THEN** the light follows the new size without a gap or a stretch

#### Scenario: Another session is shown
- **GIVEN** the light is running for the session shown
- **WHEN** the user picks an idle session or starts a new quick chat
- **THEN** the light stops at once, without the finishing lap, and an error that session already had does not pulse

#### Scenario: Reduced motion or Low Power Mode
- **GIVEN** the system asks for reduced motion or is in Low Power Mode
- **WHEN** the agent works in the panel
- **THEN** the edge is lit but does not move, and it goes off when the run ends, without a finishing lap or a pulse

### Requirement: Change the model from the panel
The panel SHALL let the user choose the model it uses, with the same picker as the main window. Before the first prompt the choice SHALL apply to the quick chat the panel is about to start, without changing the model that new sessions started elsewhere use. Inside a session the choice SHALL change that session's model, exactly as the main window does.

#### Scenario: Model for the next quick chat
- **GIVEN** the panel is open on an empty quick chat
- **WHEN** the user picks another model and sends a prompt
- **THEN** the new session runs on the picked model, and the start page's model for new sessions is unchanged

#### Scenario: Model of a session shown in the panel
- **GIVEN** the panel shows an existing session
- **WHEN** the user picks another model
- **THEN** that session's model changes, as it would in the main window

### Requirement: Requests in the panel
The panel SHALL show the pending permission and input requests of the session it shows, above its composer, and let the user answer them there.

#### Scenario: Permission while the panel is open
- **WHEN** the session in the panel asks for a permission
- **THEN** the request appears in the panel and can be allowed or denied without opening the main window

### Requirement: Open in the main window
The panel SHALL offer to open its session in the main window (also with Cmd+Enter on macOS, Ctrl+Enter elsewhere). Doing so SHALL show the session in the main window without reloading it, bring the main window to the front (reopening it if it was closed), and close the panel.

#### Scenario: Continue in the main window
- **GIVEN** a quick chat with two turns is shown in the panel
- **WHEN** the user chooses to open it in the main window
- **THEN** the main window shows the same session with both turns, and the panel closes

### Requirement: Global shortcut on every desktop platform
The shortcut SHALL default to Option+Space on macOS and Ctrl+Alt+Space on Windows and Linux, and SHALL be changeable in Settings. When the shortcut cannot be registered (taken by another application, or not supported by the desktop), Settings SHALL say so, and the panel SHALL remain reachable from the tray menu and from the `--quick` command line flag.

#### Scenario: Wayland desktop without a shortcut service
- **GIVEN** a Wayland desktop that offers no global shortcut service
- **WHEN** the user opens the quick panel settings
- **THEN** Settings says the shortcut is unavailable and shows the command `querymt-gpui --quick` to bind in the desktop's own shortcut settings

#### Scenario: Shortcut already taken
- **WHEN** the configured shortcut is registered by another application
- **THEN** Settings shows that it could not be registered, and the previous working shortcut stays active

### Requirement: Toggle from the command line
Running the app with `--quick` while it is already running SHALL toggle that instance's panel and exit without opening a second instance. When no instance runs, it SHALL start the app with only the panel open.

#### Scenario: Toggle a running instance
- **GIVEN** the app is running
- **WHEN** `querymt-gpui --quick` is run
- **THEN** the running app's panel toggles and the command exits

### Requirement: Runs in the background
While the quick panel is enabled, closing the main window SHALL NOT quit the app on any desktop platform. A tray icon SHALL offer to open the main window, toggle the quick panel, and quit. Quitting SHALL end the app even when windows are open.

#### Scenario: Main window closed on Windows
- **GIVEN** the quick panel is enabled on Windows
- **WHEN** the user closes the main window
- **THEN** the app keeps running, the shortcut still opens the panel, and the tray menu reopens the main window

#### Scenario: Panel disabled
- **GIVEN** the quick panel is disabled in Settings
- **WHEN** the user closes the main window on Windows or Linux
- **THEN** the app quits as it does today, and no tray icon is shown

### Requirement: Desktop only
The web build SHALL NOT offer the quick panel, its shortcut, the tray icon or its settings.

#### Scenario: Web build settings
- **WHEN** the user opens Settings in the web build
- **THEN** no quick panel settings are shown
