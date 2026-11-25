# mermaid-diagrams Specification

## Purpose

Draws Mermaid diagrams that appear as fenced code in transcript Markdown, so users read them as diagrams rather than source, while keeping the source one click away.

## Requirements

### Requirement: Closed Mermaid fences render as diagrams
A fenced code block whose language is `mermaid` (case-insensitive) SHALL be drawn as a diagram wherever transcript Markdown is shown, once its closing fence is present. Other code blocks SHALL be unchanged.

#### Scenario: Flowchart in a reply
- **GIVEN** a reply contains a closed fence with language `mermaid` and the body `flowchart LR; a --> b`
- **WHEN** the reply is shown
- **THEN** a diagram with two nodes `a` and `b` joined by an arrow is drawn in place of the code

#### Scenario: Other languages untouched
- **GIVEN** a reply contains a closed fence with language `rust`
- **WHEN** the reply is shown
- **THEN** it is shown as a highlighted code block, as before

### Requirement: Open fences stay code while streaming
A `mermaid` fence without its closing fence SHALL be shown as a code block. It SHALL switch to a diagram once the closing fence arrives, and SHALL NOT show a partial diagram.

#### Scenario: Diagram streams in
- **GIVEN** a reply is streaming and has sent the opening fence with language `mermaid` and two lines of its body
- **WHEN** the reply is shown
- **THEN** those lines are shown as a code block
- **AND WHEN** the closing fence arrives
- **THEN** the block is drawn as a diagram

### Requirement: Diagram card header
A diagram SHALL be shown in a card with the same frame and header as code blocks. The header SHALL show `mermaid`, a toggle between Diagram and Source, and a copy button. While the diagram is shown, the header SHALL also show zoom controls: zoom out, the current zoom as a percentage, zoom in, and Fit. The zoom controls SHALL be hidden in Source mode and in the fallback. Copy SHALL put the Mermaid source on the clipboard in either mode.

#### Scenario: Copy the source
- **GIVEN** a diagram is shown
- **WHEN** the user clicks its copy button
- **THEN** the clipboard holds the fence's body exactly, without the fence lines

#### Scenario: Toggle to source
- **GIVEN** a diagram is shown
- **WHEN** the user picks Source
- **THEN** the card shows the Mermaid source as a code block, and picking Diagram shows the diagram again

### Requirement: Card state survives scrolling
The Diagram or Source choice for a block, and its zoom and pan, SHALL persist while the session stays open, including when its row scrolls out of view and back.

#### Scenario: Scroll away and back
- **GIVEN** the user switched a diagram to Source
- **WHEN** the row scrolls out of view and back into view
- **THEN** the card still shows Source

#### Scenario: Zoomed diagram scrolls away and back
- **GIVEN** the user zoomed a diagram to 200% and panned it
- **WHEN** the row scrolls out of view and back into view
- **THEN** the diagram is still at 200% with the same part of it in view

### Requirement: Diagrams start fitted to the card
A diagram SHALL first be drawn at the largest uniform scale, no greater than 1, at which it fits both the width of the transcript column and a height of 480px. It SHALL NOT be scaled up. The diagram area's height SHALL be the fitted diagram's height. Resizing the window SHALL rescale the diagram without changing its layout.

#### Scenario: Wide diagram
- **GIVEN** a diagram whose natural width is twice the column's width and whose natural height is under 960px
- **WHEN** it is shown
- **THEN** it is drawn at half size, fully visible horizontally, and the card's height is half the diagram's natural height plus the header

#### Scenario: Small diagram
- **GIVEN** a diagram narrower than the column and shorter than 480px
- **WHEN** it is shown
- **THEN** it is drawn at its natural size, not stretched

#### Scenario: Tall diagram
- **GIVEN** a diagram narrower than the column whose natural height is 1200px
- **WHEN** it is shown
- **THEN** the diagram area is 480px tall and the whole diagram is visible at 40% scale

### Requirement: Zoom a diagram
The user SHALL be able to zoom a shown diagram with Cmd+wheel (Ctrl+wheel on other platforms), a trackpad pinch, or the zoom controls. Wheel and pinch zoom SHALL keep the point under the pointer in place. Zoom SHALL range from the fitted scale up to the greater of four times the fitted scale and twice the natural size. Fit or a double-click SHALL return to the fitted scale.

#### Scenario: Modifier-wheel zoom
- **GIVEN** a diagram is shown at Fit with the pointer over one of its nodes
- **WHEN** the user scrolls up with Cmd held
- **THEN** the diagram zooms in and the node stays under the pointer

#### Scenario: Pinch zoom
- **GIVEN** a diagram is shown
- **WHEN** the user pinches outward on the trackpad over it
- **THEN** the diagram zooms in around the pinch point

#### Scenario: Back to Fit
- **GIVEN** a zoomed and panned diagram
- **WHEN** the user clicks Fit or double-clicks the diagram
- **THEN** the diagram is shown fitted and centred again

#### Scenario: Zoom limits
- **GIVEN** a diagram at Fit
- **WHEN** the header is shown
- **THEN** zoom out and Fit are disabled, and zoom in is enabled until the maximum zoom, where it is disabled

### Requirement: Pan a zoomed diagram
The user SHALL be able to pan a diagram zoomed past Fit by dragging it. Panning SHALL stop at the diagram's edges, a diagram smaller than its area SHALL stay centred on that axis, and dragging SHALL NOT start a text selection.

#### Scenario: Drag to pan
- **GIVEN** a diagram zoomed past Fit
- **WHEN** the user drags it with the mouse
- **THEN** the diagram moves with the pointer, no text is selected, and it stops when an edge of the diagram reaches the edge of its area

### Requirement: Zoom and pan leave the transcript alone
A wheel or trackpad scroll without the zoom modifier over a diagram SHALL scroll the transcript and SHALL NOT change the diagram. Zooming or panning SHALL NOT scroll the transcript or change the card's height.

#### Scenario: Plain scroll passes through
- **GIVEN** a zoomed diagram is shown with the pointer over it
- **WHEN** the user scrolls without a modifier
- **THEN** the transcript scrolls and the diagram's zoom and pan are unchanged

#### Scenario: Zooming does not scroll the transcript
- **GIVEN** a diagram is shown with the pointer over it
- **WHEN** the user scrolls with Cmd held or pinches
- **THEN** the transcript does not move

#### Scenario: Zoom keeps the row height
- **GIVEN** a diagram is shown
- **WHEN** the user zooms in or out
- **THEN** the card's height does not change and rows below it do not move

### Requirement: Diagrams follow the app's theme and fonts
Diagrams SHALL be drawn in the app's interface font and with colours from the current theme, readable in light and dark mode. Switching the theme SHALL recolour shown diagrams without changing their layout. Colours the diagram's author set SHALL be ignored.

#### Scenario: Switch to dark mode
- **GIVEN** a diagram is shown in light mode
- **WHEN** the user switches to dark mode
- **THEN** the diagram's text, lines and node fills use the dark theme's colours, and the diagram's size is unchanged

### Requirement: Fall back to code when a diagram cannot be drawn
When the source of a closed `mermaid` fence does not parse, names a diagram type the renderer does not support, or exceeds the renderer's limits, the block SHALL be shown as a code block with a one-line note saying the diagram could not be drawn and why. The note SHALL NOT replace the source, and the copy button SHALL still copy it.

#### Scenario: Syntax error
- **GIVEN** a closed `mermaid` fence with body `flowchart LR; a -->`
- **WHEN** it is shown
- **THEN** the source is shown as a code block with a note that the diagram could not be drawn

#### Scenario: Unsupported type
- **GIVEN** a closed `mermaid` fence whose diagram type the renderer does not support
- **WHEN** it is shown
- **THEN** the source is shown as a code block with a note naming the unsupported type

### Requirement: Diagrams do not disturb the transcript
A diagram's height SHALL be known when its row is first laid out, so the transcript does not jump when a diagram appears. Laying out a diagram SHALL happen once per source and font, not on every frame.

#### Scenario: Scrolling past a diagram
- **GIVEN** a session with a large diagram above the viewport
- **WHEN** the user scrolls up past it
- **THEN** the content does not jump as the diagram's row is built
