//! The quick panel window. A new quick chat is one bar: the input, the model
//! and the session picker. A conversation is a compact session pane under a
//! header with a new-chat button, the session picker and "open in main
//! window".

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Enter, InputEvent, InputState, MoveDown, MoveUp, Textarea};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::activity::{self, Activity, Observation, Tracker};
use super::chrome::{self, Chrome};
use super::edge::{self, Edge, EdgeColours, EdgeFrame, EdgeLight, EdgeState, MOTION, Outline};
use super::hud::{PROGRESS_FILL_ALPHA, hud_palette, progress_colour};
use super::{Picks, Resume};
use crate::session::PlanDigest;
use crate::session::open::{OpenSession, SessionKey, SessionRef};
use crate::store::launch::{LaunchError, LaunchRequest, Placement};
use crate::store::{AgentsStore, LAUNCH_REASONING, PickerLayout, QuickPosition, SessionSummary};
use crate::theme::{Palette, palette};
use crate::ui::keys::{ChooseFolder, CycleReasoning, OpenModelPicker, OpenSidebarSession};
use crate::ui::model_picker::{ModelPicker, ModelPickerEvent, PickerFrame, Target};
use crate::ui::workspace_select::{WorkspaceEvent, WorkspaceField, home_dir};
use crate::ui::{Density, SessionPane, SessionPaneEvent, SmoothScroll, kit};

/// Width of the panel window.
pub(crate) const WIDTH: f32 = 680.;
/// Height of a new quick chat: its bar, one line of text (design D15).
pub(super) const BAR_HEIGHT: f32 = 64.;
/// Size of the text typed into the bar.
const BAR_TEXT: f32 = 18.;
/// Height of its lines, when more than one is typed.
const BAR_LINE: f32 = 26.;
/// The least the panel is tall around a conversation.
pub(super) const MIN_HEIGHT: f32 = 260.;
/// The panel never grows past this, nor past [`DISPLAY_SHARE`] of its display.
const MAX_HEIGHT: f32 = 760.;
const DISPLAY_SHARE: f32 = 0.7;
/// Heights are requested in steps of this many px, so a streamed chunk does
/// not resize the window every time.
const STEP: f32 = 8.;
/// The model picker's card needs about this much room.
const MODEL_PICKER_FLOOR: f32 = 520.;
/// Room for the folder chooser and its list of suggestions.
const FOLDER_FLOOR: f32 = 440.;
/// Height of the header of a conversation.
const HEADER_HEIGHT: f32 = 44.;
/// How long the panel must stay put before where it was left is written.
const POSITION_SAVE_MS: u64 = 500;
/// Share of its display an overlay may make the window.
const OVERLAY_SHARE: f32 = 0.9;
/// What the session list needs besides its rows before it is laid out:
/// header, search, padding. Only an estimate, for the first frame.
const PICKER_CHROME: f32 = HEADER_HEIGHT + 52. + 24.;
const PICKER_ROW: f32 = 46.;
/// A card of the picker's grid with every line it can have: title, meta,
/// bar, three todos and "+N more".
const GRID_CARD: f32 = 148.;
const GRID_GAP: f32 = 8.;
const GRID_COLUMNS: usize = 2;
/// Opening animation (design D14): the whole of it.
const OPEN_MS: u64 = 280;
/// The panel starts this much smaller than its final size.
const OPEN_SCALE_FROM: f32 = 0.94;
/// Share of the animation after which the panel is at its full size and
/// opacity. The rest is the bump.
const OPEN_ARRIVE: f32 = 0.64;
/// How far the panel springs back after it arrives, as a share of its size.
const OPEN_BUMP: f32 = 0.014;
/// 1 / the peak of `sin(pi v) (1 - v)`, the shape of the bump.
const BUMP_NORM: f32 = 1.727;
/// Sessions the picker lists at most.
const PICKER_ROWS: usize = 8;

/// What the panel tells the host.
pub(crate) enum QuickPanelEvent {
    /// The window closed; the host keeps what to show next time.
    Closed {
        session: Option<Entity<OpenSession>>,
        draft: String,
        /// Model, folder and reasoning picked for the next quick chat.
        picks: Picks,
    },
    /// Show the session in the main window. `then` is an app-level request
    /// of the pane that the main window takes up afterwards.
    OpenInMain {
        target: SessionRef,
        then: Option<SessionPaneEvent>,
    },
}

pub(crate) struct QuickPanel {
    store: Entity<AgentsStore>,
    /// What the window's material is, and so what its surfaces are painted as.
    chrome: Chrome,
    /// Height asked of the window (design D13).
    height: f32,
    /// Viewport height of the transcript when the window was last resized:
    /// until it changes, the layout is older than the resize and its overflow
    /// must not be acted on twice.
    settling: Option<(EntityId, f32)>,
    /// The display's height, when a test fixes it.
    display_height: Option<f32>,
    /// Last value of the opening animation, 0 to 1.
    opening: Rc<Cell<f32>>,
    /// Opacity and scale last given to a material of ours ([`chrome::open_step`]).
    stepped: Rc<Cell<Option<(f32, f32)>>>,
    /// Height the bar of a new quick chat needs, as last laid out: it grows
    /// with the lines typed and with an error shown above it.
    bar_h: Rc<Cell<f32>>,
    /// What the open overlays need, as last laid out (design D13): the session
    /// list's search row and its rows, the model picker's card and the folder
    /// chooser's card. 0 while not laid out; the fixed estimates then stand in.
    list_search_h: Rc<Cell<f32>>,
    list_rows_h: Rc<Cell<f32>>,
    model_h: Rc<Cell<f32>>,
    folder_h: Rc<Cell<f32>>,
    /// What the shown session is doing, for the light along the edge.
    tracker: Tracker,
    /// What the light shows (design D14).
    edge: Edge,
    /// The activity last read, and whether the system was in Low Power Mode
    /// then: it is read at each change of activity.
    last_activity: Activity,
    low_power: bool,
    /// A renderer of the light outside GPUI; `None`, or after any of its
    /// calls failed, the panel draws the ring itself.
    light: Option<Box<dyn EdgeLight>>,
    /// What the renderer was last told: its size and its colours.
    light_size: Option<(f32, f32)>,
    light_colours: Option<EdgeColours>,
    /// The frame the ring last drew; `None` while it is not drawn.
    ring_frame: Rc<Cell<Option<EdgeFrame>>>,
    /// A render at the edge's next change in time, and when.
    wake: Option<(f64, Task<()>)>,
    pane: Entity<SessionPane>,
    query: Entity<InputState>,
    picker_open: bool,
    /// The row of the session list the keyboard is on.
    picker_cursor: usize,
    /// The model picker, drawn inside the panel (design D12).
    models: Entity<ModelPicker>,
    /// The folder chooser of a new quick chat, drawn inside the panel.
    folders: Entity<WorkspaceField>,
    folder_input: Entity<InputState>,
    folder_open: bool,
    /// What was picked for the next quick chat. It is the panel's own: the
    /// start page's launch defaults are not touched by it (the folder is
    /// remembered by the launch, as for any session).
    picks: Picks,
    /// The quick chat being created is to be shown in the main window.
    open_after_launch: bool,
    /// The model picker was open at the last notification.
    models_were_open: bool,
    /// The quick chat being created.
    launching: Option<Task<()>>,
    error: Option<String>,
    /// The window has been active once: only then does losing focus close it.
    active_seen: bool,
    closed: bool,
    /// The window can be dragged (not a layer-shell surface, design D18).
    movable: bool,
    /// Writes where the panel was left once it stops moving.
    position_save: Option<Task<()>>,
    /// Drags started, for tests.
    #[cfg(test)]
    drags: usize,
    _subs: Vec<Subscription>,
}

impl EventEmitter<QuickPanelEvent> for QuickPanel {}

/// Window kinds to try, best first (design D7).
fn window_kinds() -> Vec<WindowKind> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        vec![WindowKind::PopUp]
    }
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            use gpui_kit::layer_shell::{KeyboardInteractivity, Layer, LayerShellOptions};
            // wlroots and KDE have layer-shell; GNOME refuses it and gets a
            // normal window.
            vec![
                WindowKind::LayerShell(LayerShellOptions {
                    namespace: "querymt-quick".into(),
                    layer: Layer::Overlay,
                    keyboard_interactivity: KeyboardInteractivity::Exclusive,
                    ..Default::default()
                }),
                WindowKind::Normal,
            ]
        } else {
            // Not `PopUp`: override-redirect windows get no keyboard focus
            // from the window manager.
            vec![WindowKind::Floating, WindowKind::Normal]
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        vec![WindowKind::Normal]
    }
}

/// The height the panel opens at.
fn initial_height(session: bool) -> f32 {
    if session { MIN_HEIGHT } else { BAR_HEIGHT }
}

/// Whether a window of `kind` can be dragged: a layer-shell surface is placed
/// by the compositor (design D18).
fn movable(kind: &WindowKind) -> bool {
    #[cfg(target_os = "linux")]
    if matches!(kind, WindowKind::LayerShell(_)) {
        return false;
    }
    let _ = kind;
    true
}

/// The edge the platform keeps while the panel grows, and so the one kept
/// where the user left it: the bottom on macOS (design D13), else the top.
pub(crate) const BOTTOM_ANCHORED: bool = cfg!(target_os = "macos");

/// What a display is known by in the stored positions: its uuid, which
/// outlives a restart, else its id.
pub(crate) fn display_key(display: &dyn PlatformDisplay) -> String {
    display
        .uuid()
        .map(|uuid| uuid.to_string())
        .unwrap_or_else(|_| format!("id:{:?}", display.id()))
}

/// Where the panel was left on a display whose bounds are `display`.
pub(crate) fn position_of(bounds: Bounds<Pixels>, display: Bounds<Pixels>) -> QuickPosition {
    let rel = |v: Pixels, origin: Pixels| f32::from(v - origin).round() as i32;
    QuickPosition {
        left: rel(bounds.left(), display.left()),
        top: rel(bounds.top(), display.top()),
        bottom: rel(bounds.bottom(), display.top()),
    }
}

/// The bounds of a panel `height` tall left at `position` on a display:
/// the kept edge (`bottom_anchored`) where it was, the whole panel inside the
/// display's `visible` area (a display may have shrunk since).
pub(crate) fn placed(
    display: Bounds<Pixels>,
    visible: Bounds<Pixels>,
    position: QuickPosition,
    height: f32,
    bottom_anchored: bool,
) -> Bounds<Pixels> {
    let (dx, dy) = (f32::from(display.left()), f32::from(display.top()));
    let top = if bottom_anchored {
        dy + position.bottom as f32 - height
    } else {
        dy + position.top as f32
    };
    let within = |v: f32, low: Pixels, high: Pixels, size: f32| {
        let (low, high) = (f32::from(low), f32::from(high) - size);
        v.min(high).max(low)
    };
    let left = within(
        dx + position.left as f32,
        visible.left(),
        visible.right(),
        WIDTH,
    );
    let top = within(top, visible.top(), visible.bottom(), height);
    Bounds::new(point(px(left), px(top)), size(px(WIDTH), px(height)))
}

fn window_options(
    kind: WindowKind,
    display: Option<DisplayId>,
    height: f32,
    positions: &std::collections::BTreeMap<String, QuickPosition>,
    cx: &App,
) -> WindowOptions {
    // Placed where a centred conversation has its bottom edge: the panel
    // grows upward from there (design D13), so the bar leaves it room.
    let mut bounds = Bounds::centered(display, size(px(WIDTH), px(MIN_HEIGHT)), cx);
    bounds.origin.y += px(MIN_HEIGHT - height);
    bounds.size.height = px(height);
    // Where the user left it on this display, if they moved it (design D18).
    let movable = movable(&kind);
    let screen = display
        .and_then(|id| cx.find_display(id))
        .or_else(|| cx.primary_display());
    if movable
        && let Some(screen) = screen
        && let Some(position) = positions.get(&display_key(&*screen))
    {
        bounds = placed(
            screen.bounds(),
            screen.visible_bounds(),
            *position,
            height,
            BOTTOM_ANCHORED,
        );
    }
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: None,
        focus: true,
        show: true,
        kind,
        // AppKit and Windows move only a movable window.
        is_movable: movable,
        is_resizable: false,
        is_minimizable: false,
        display_id: display,
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("querymt-gpui".into()),
        ..Default::default()
    }
}

/// Open the panel window centred on `display` (the main window's, else the
/// primary one).
pub(super) fn open_window(
    store: Entity<AgentsStore>,
    resume: Resume,
    display: Option<DisplayId>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<QuickPanel>)> {
    let mut last = None;
    let height = initial_height(resume.session.is_some());
    for kind in window_kinds() {
        let movable = movable(&kind);
        let positions = store.read(cx).prefs.quick.positions.clone();
        let options = window_options(kind, display, height, &positions, cx);
        let (store, resume) = (store.clone(), resume.clone());
        let mut built = None;
        // Not `gpui_kit::open_window`: its `Root` is painted with the app
        // theme's opaque background (gpui-component's root plugin), which
        // would cover the material and every layer under GPUI's view. The
        // root's own style is applied after the plugin's, so it can clear it.
        match cx.open_window(options, |window, cx| {
            let panel = cx.new(|cx| QuickPanel::new(store, resume, movable, window, cx));
            built = Some(panel.clone());
            cx.new(|cx| gpui_kit::base::Root::new(panel, window, cx).bg(transparent_black()))
        }) {
            Ok(handle) => {
                return Ok((
                    handle.into(),
                    built.expect("open_window ran its build closure"),
                ));
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last.expect("at least one window kind"))
}

impl QuickPanel {
    fn new(
        store: Entity<AgentsStore>,
        resume: Resume,
        movable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let Resume {
            session,
            draft,
            picks,
        } = resume;
        // Under reduced motion GPUI draws the animation's last frame at once.
        let chrome = chrome::install(window, !cx.reduce_motion());
        let hud = hud_palette(palette(cx), chrome.glass);
        let height = initial_height(session.is_some());
        let pane = {
            let store = store.clone();
            cx.new(|cx| {
                let mut pane = SessionPane::with_density(
                    store,
                    SmoothScroll::default(),
                    Density::Compact,
                    window,
                    cx,
                );
                pane.set_glass(chrome.glass, cx);
                pane.set_palette(Some(hud), cx);
                pane
            })
        };
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search sessions…"));
        // Reasoning effort of a new quick chat is the launch default, which the
        // panel does not change.
        let models = {
            let store = store.clone();
            cx.new(|cx| {
                let mut models = ModelPicker::new(store, SmoothScroll::default(), window, cx)
                    .with_launch_reasoning(false);
                models.set_palette(Some(hud));
                models
            })
        };
        let folder_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Folder for this chat…"));
        let folders = {
            let (store, input) = (store.clone(), folder_input.clone());
            cx.new(|cx| {
                let mut field =
                    WorkspaceField::new(store, input, true, SmoothScroll::default(), cx);
                field.set_palette(Some(hud), cx);
                // In the panel's layout, so the window makes room for them.
                field.set_inline(true);
                field
            })
        };
        pane.update(cx, |pane, cx| {
            if let Some(session) = session {
                pane.show(session, cx);
            }
            pane.set_draft(&draft, window, cx);
            pane.focus_composer(window, cx);
        });
        let subs = vec![
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.observe(&pane, |_, _, cx| cx.notify()),
            cx.subscribe_in(&pane, window, Self::on_pane_event),
            cx.observe_in(&models, window, |this, models, window, cx| {
                // Closing the picker gives the keyboard back to the composer.
                let open = models.read(cx).is_open();
                if std::mem::replace(&mut this.models_were_open, open) && !open {
                    this.pane
                        .update(cx, |pane, cx| pane.focus_composer(window, cx));
                }
                cx.notify();
            }),
            cx.subscribe(&models, |this, _, event: &ModelPickerEvent, cx| {
                let ModelPickerEvent::Picked { key } = event;
                // Before the first prompt the choice is for the next quick
                // chat; in a session the picker already changed its model.
                if this.pane.read(cx).session().is_none() {
                    this.picks.model = Some(key.clone());
                    this.models.update(cx, |models, cx| {
                        models.close(cx);
                    });
                }
                cx.notify();
            }),
            cx.subscribe_in(
                &query,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::Change => {
                        this.picker_cursor = 0;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.pick_cursor(window, cx),
                    _ => {}
                },
            ),
            // A folder chosen from the suggestions, or typed in full.
            cx.subscribe_in(
                &folders,
                window,
                |this, _, ev: &WorkspaceEvent, window, cx| {
                    let WorkspaceEvent::Picked(path) = ev;
                    this.choose_folder(path, window, cx);
                },
            ),
            cx.subscribe_in(
                &folder_input,
                window,
                |this, input, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) && this.folder_open {
                        let path = input.read(cx).value().to_string();
                        this.choose_folder(&path, window, cx);
                    }
                },
            ),
            cx.observe_window_activation(window, |this, window, cx| {
                this.on_activation(window.is_window_active(), window, cx)
            }),
            cx.observe_window_bounds(window, |this, window, cx| this.bounds_changed(window, cx)),
        ];
        let edge = Edge::new(cx.background_executor().now());
        // Over the panel's own material the light is Core Animation's; when
        // there is none, or it fails, the ring is drawn.
        let light = chrome::edge_light(&chrome, window, edge.origin());
        Self {
            store,
            chrome,
            height,
            settling: None,
            display_height: None,
            opening: Rc::new(Cell::new(0.)),
            stepped: Rc::new(Cell::new(None)),
            bar_h: Rc::new(Cell::new(0.)),
            list_search_h: Rc::new(Cell::new(0.)),
            list_rows_h: Rc::new(Cell::new(0.)),
            model_h: Rc::new(Cell::new(0.)),
            folder_h: Rc::new(Cell::new(0.)),
            tracker: Tracker::new(cx.background_executor().now()),
            edge,
            last_activity: Activity::default(),
            low_power: false,
            light,
            light_size: None,
            light_colours: None,
            ring_frame: Rc::new(Cell::new(None)),
            wake: None,
            pane,
            query,
            picker_open: false,
            picker_cursor: 0,
            models,
            folders,
            folder_input,
            folder_open: false,
            picks,
            open_after_launch: false,
            models_were_open: false,
            launching: None,
            error: None,
            active_seen: window.is_window_active(),
            closed: false,
            movable,
            position_save: None,
            #[cfg(test)]
            drags: 0,
            _subs: subs,
        }
    }

    /// The window gained or lost focus. Losing it after having had it
    /// dismisses the panel.
    fn on_activation(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.active_seen = true;
        } else if self.active_seen {
            self.dismiss(window, cx);
        }
    }

    /// The window moved or grew: remember where it is on its display, and
    /// write it down once it stays put (design D18).
    fn bounds_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.movable || self.closed {
            return;
        }
        let Some(display) = window.display(cx) else {
            return;
        };
        let position = position_of(window.bounds(), display.bounds());
        let key = display_key(&*display);
        let store = self.store.clone();
        store.update(cx, |s, _| s.remember_quick_position(key, position));
        // Replacing the task cancels a write still waiting.
        self.position_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(POSITION_SAVE_MS))
                .await;
            let _ = this.update(cx, |this, cx| this.flush_position(cx));
        }));
    }

    /// Write a remembered position that is still waiting.
    fn flush_position(&mut self, cx: &mut Context<Self>) {
        if self.position_save.take().is_some() {
            self.store.read(cx).save();
        }
    }

    /// A press on a drag area: move the window with the pointer (design D18).
    fn start_drag(&mut self, window: &mut Window) {
        // GPUI's test window does not implement moving: tests count instead.
        #[cfg(test)]
        {
            let _ = window;
            self.drags += 1;
        }
        #[cfg(not(test))]
        if self.movable {
            window.start_window_move();
        }
    }

    /// Close the window; the host keeps the session and the draft.
    pub(crate) fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        self.flush_position(cx);
        let (session, draft) = {
            let pane = self.pane.read(cx);
            (pane.session().cloned(), pane.draft(cx))
        };
        cx.emit(QuickPanelEvent::Closed {
            session,
            draft,
            picks: self.picks.clone(),
        });
        window.remove_window();
    }

    /// Close the window after its session moved to the main window: nothing
    /// is kept.
    pub(crate) fn close_handed_off(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.closed, true) {
            return;
        }
        self.flush_position(cx);
        self.pane.update(cx, |pane, cx| pane.clear(cx));
        self.pane
            .update(cx, |pane, cx| pane.set_draft("", window, cx));
        cx.emit(QuickPanelEvent::Closed {
            session: None,
            draft: String::new(),
            picks: self.picks.clone(),
        });
        window.remove_window();
    }

    fn on_pane_event(
        &mut self,
        _: &Entity<SessionPane>,
        event: &SessionPaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Back with nothing left to go back to: a fresh quick chat.
            SessionPaneEvent::Leave => self.new_chat(window, cx),
            SessionPaneEvent::OpenInMain => self.open_in_main(None, cx),
            // The picker is drawn inside the panel.
            SessionPaneEvent::ModelPicker => self.open_model_picker(window, cx),
            // The panel has no dialogs, lightbox or settings: the main window
            // takes the request after showing the session.
            other => self.open_in_main(Some(other.clone()), cx),
        }
    }

    /// Open the model picker on the session shown, or on the quick chat the
    /// panel is about to start.
    pub(crate) fn open_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = match self.pane.read(cx).session().cloned() {
            Some(session) => Target::Session(session),
            None => match self.store.read(cx).primary_agent() {
                Some(agent) => Target::Launch { agent },
                None => {
                    self.error = Some(LaunchError::NotConnected.to_string());
                    cx.notify();
                    return;
                }
            },
        };
        self.picker_open = false;
        self.folder_open = false;
        self.models
            .update(cx, |models, cx| models.open(target, window, cx));
    }

    /// Ask the host to show the session in the main window.
    pub(crate) fn open_in_main(&mut self, then: Option<SessionPaneEvent>, cx: &mut Context<Self>) {
        let Some(target) = self.pane.read(cx).current_ref(cx) else {
            return;
        };
        cx.emit(QuickPanelEvent::OpenInMain { target, then });
    }

    /// Leave the session shown and start over on an empty quick chat.
    pub(crate) fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.launching = None;
        self.open_after_launch = false;
        self.error = None;
        self.picker_open = false;
        self.folder_open = false;
        self.pane.update(cx, |pane, cx| {
            pane.clear(cx);
            pane.focus_composer(window, cx);
        });
        // Back to the bar.
        self.height = BAR_HEIGHT;
        self.settling = None;
        chrome::resize(window, size(px(WIDTH), px(self.height)), cx);
        cx.notify();
    }

    /// Height of the display the panel is on.
    fn display_height(&self, window: &Window, cx: &App) -> f32 {
        self.display_height
            .or_else(|| {
                window
                    .display(cx)
                    .map(|d| f32::from(d.bounds().size.height))
            })
            .unwrap_or(900.)
    }

    /// The least height that shows what is open in place of the conversation.
    ///
    /// What is open is measured (design D13): what it laid out last frame is
    /// the floor. The fixed estimates stand in only until the first layout.
    fn height_floor(&self, cx: &App) -> f32 {
        let shown = self.pane.read(cx).session().is_some();
        // The root's border, top and bottom.
        const BORDER: f32 = 2.;
        // A new quick chat is as tall as its bar needs.
        let bar = if shown {
            0.
        } else {
            self.bar_h.get().ceil() + BORDER
        };
        // What is open takes the conversation's place, under the header; in a
        // new quick chat it lies above the bar, which holds the border.
        let around = if shown { HEADER_HEIGHT + BORDER } else { bar };
        let placed = |measured: Option<f32>, estimate: f32| match measured {
            Some(h) => h.ceil() + around,
            None => estimate + if shown { 0. } else { BAR_HEIGHT },
        };
        let sessions = if self.picker_open {
            let (search, rows) = (self.list_search_h.get(), self.list_rows_h.get());
            let estimate = picker_height(self.picker_layout(cx), self.picker_rows(cx).len());
            placed((rows > 0.).then_some(search + rows), estimate)
        } else {
            self.list_search_h.set(0.);
            self.list_rows_h.set(0.);
            0.
        };
        let models = if self.models.read(cx).is_open() {
            let h = self.model_h.get();
            placed((h > 0.).then_some(h), MODEL_PICKER_FLOOR)
        } else {
            self.model_h.set(0.);
            0.
        };
        let folders = if self.folder_open {
            let h = self.folder_h.get();
            placed((h > 0.).then_some(h), FOLDER_FLOOR)
        } else {
            self.folder_h.set(0.);
            0.
        };
        sessions.max(models).max(folders).max(bar)
    }

    /// Make room for a conversation before it is shown, so it is not laid out
    /// in the bar's height first.
    fn make_room(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.height < MIN_HEIGHT {
            self.height = MIN_HEIGHT;
            self.settling = None;
            chrome::resize(window, size(px(WIDTH), px(self.height)), cx);
        }
    }

    /// What [`fitted_height`] asks for now, if it differs from the window.
    fn wanted_height(&self, window: &Window, cx: &App) -> Option<f32> {
        let pane = self.pane.read(cx);
        let (viewport, overflow) = pane.content_metrics();
        // Only a transcript has a layout that can be out of date.
        let shown = pane.session().map(|s| s.entity_id());
        if shown.is_some()
            && self
                .settling
                .is_some_and(|(id, v)| Some(id) == shown && (v - viewport).abs() < 0.5)
        {
            return None;
        }
        let wanted = fitted_height(
            self.height,
            overflow,
            self.height_floor(cx),
            self.display_height(window, cx),
            pane.session().is_some(),
        );
        ((wanted - self.height).abs() >= 0.5).then_some(wanted)
    }

    /// Resize the window to what the conversation and the open overlays
    /// need. Runs after layout, never while painting.
    fn fit_height(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(wanted) = self.wanted_height(window, cx) else {
            return;
        };
        let pane = self.pane.read(cx);
        self.settling = pane
            .session()
            .map(|s| (s.entity_id(), pane.content_metrics().0));
        self.height = wanted;
        chrome::resize(window, size(px(WIDTH), px(wanted)), cx);
        cx.notify();
    }

    /// Show `key` in the panel; the main window is not touched.
    pub(crate) fn pick(
        &mut self,
        summary: &SessionSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = SessionRef {
            key: SessionKey::new(&summary.agent_id, &summary.id),
            cwd: summary.cwd.clone(),
            title: summary.display_title(),
        };
        self.picker_open = false;
        self.error = None;
        self.make_room(window, cx);
        self.pane.update(cx, |pane, cx| {
            pane.open(target, cx);
            pane.focus_composer(window, cx);
        });
        cx.notify();
    }

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = !self.picker_open;
        if self.picker_open {
            self.picker_cursor = 0;
            self.folder_open = false;
            self.query.update(cx, |q, cx| q.focus(window, cx));
        } else {
            self.pane
                .update(cx, |pane, cx| pane.focus_composer(window, cx));
        }
        cx.notify();
    }

    /// Show the session in row `ix` of the open list (Cmd/Ctrl+1…8). False
    /// when the list is closed or has no such row.
    fn pick_nth(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.picker_open {
            return false;
        }
        let Some(row) = self.picker_rows(cx).into_iter().nth(ix) else {
            return false;
        };
        self.pick(&row.summary, window, cx);
        true
    }

    /// Enter in the list's search field: the row the keyboard is on.
    fn pick_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.picker_cursor;
        self.pick_nth(at, window, cx);
    }

    /// Up and Down in the list's search field; wraps around.
    fn move_picker_cursor(&mut self, down: bool, cx: &mut Context<Self>) -> bool {
        let len = self.picker_rows(cx).len();
        if !self.picker_open || len == 0 {
            return false;
        }
        let at = self.picker_cursor.min(len - 1);
        self.picker_cursor = if down {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        };
        cx.notify();
        true
    }

    /// The folder the next quick chat runs in: the panel's choice, else the
    /// last one used. Empty when there is none yet.
    fn chosen_cwd(&self, cx: &App) -> String {
        self.picks
            .cwd
            .clone()
            .unwrap_or_else(|| self.store.read(cx).prefs.cwd.trim().to_string())
    }

    /// Open the folder chooser (Cmd/Ctrl+O), before the first prompt.
    pub(crate) fn open_folder_chooser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pane.read(cx).session().is_some() {
            return;
        }
        self.picker_open = false;
        self.models.update(cx, |models, cx| {
            models.close(cx);
        });
        let cwd = self.chosen_cwd(cx);
        self.folder_input.update(cx, |input, cx| {
            input.set_value(cwd, window, cx);
            input.focus(window, cx);
        });
        self.folder_open = true;
        cx.notify();
    }

    /// Close the folder chooser; true when it was open.
    fn close_folder_chooser(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let was = std::mem::replace(&mut self.folder_open, false);
        if was {
            self.pane
                .update(cx, |pane, cx| pane.focus_composer(window, cx));
            cx.notify();
        }
        was
    }

    /// Run the next quick chat in `path` (`~` is the home folder).
    pub(crate) fn choose_folder(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = path.trim();
        if path.is_empty() {
            return;
        }
        let expanded = match (path.strip_prefix('~'), home_dir()) {
            (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                format!("{home}{rest}")
            }
            _ => path.to_string(),
        };
        let trimmed = expanded.trim_end_matches('/');
        self.picks.cwd = Some(if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        });
        self.error = None;
        self.folder_open = false;
        self.pane
            .update(cx, |pane, cx| pane.focus_composer(window, cx));
        cx.notify();
    }

    /// The reasoning effort the next quick chat starts with.
    fn reasoning_now(&self, cx: &App) -> String {
        self.picks
            .reasoning
            .clone()
            .unwrap_or_else(|| self.store.read(cx).prefs.reasoning().to_string())
    }

    /// Cmd/Ctrl+T: the next reasoning effort, for the session shown or for
    /// the next quick chat.
    pub(crate) fn cycle_reasoning(&mut self, cx: &mut Context<Self>) {
        if self.pane.read(cx).session().is_some() {
            self.pane.update(cx, |pane, cx| pane.cycle_reasoning(cx));
            return;
        }
        let current = self.reasoning_now(cx);
        let at = LAUNCH_REASONING
            .iter()
            .position(|(id, ..)| *id == current)
            .unwrap_or(0);
        let next = LAUNCH_REASONING[(at + 1) % LAUNCH_REASONING.len()].0;
        self.picks.reasoning = Some(next.to_string());
        cx.notify();
    }

    /// Cmd/Ctrl+Enter: start the quick chat and show it in the main window.
    pub(crate) fn submit_and_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
        self.open_after_launch = self.launching.is_some();
    }

    /// Start a quick chat with the composer's text on the primary agent
    /// (design D5: launch defaults, local placement).
    pub(crate) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.launching.is_some() || self.pane.read(cx).session().is_some() {
            return;
        }
        let prompt = self.pane.read(cx).draft(cx).trim().to_string();
        if prompt.is_empty() {
            return;
        }
        let store = self.store.read(cx);
        let Some(agent) = store.primary_agent() else {
            self.error = Some(LaunchError::NotConnected.to_string());
            cx.notify();
            return;
        };
        let cwd = self.chosen_cwd(cx);
        if cwd.is_empty() {
            self.error = Some("Choose a working folder for this chat first.".into());
            self.open_folder_chooser(window, cx);
            return;
        }
        self.error = None;
        let attachments = self.pane.update(cx, |pane, _| pane.take_attachments());
        let request = LaunchRequest {
            agent_id: agent,
            cwd,
            prompt,
            attachments,
            placement: Placement::local(),
            model: self.picks.model.clone(),
            reasoning: self.picks.reasoning.clone(),
        };
        let launched = self.store.update(cx, |s, cx| s.launch(request, cx));
        self.launching = Some(cx.spawn_in(window, async move |this, cx| {
            let result = launched.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_launch(result, window, cx)
            });
        }));
        cx.notify();
    }

    fn finish_launch(
        &mut self,
        result: Result<Entity<OpenSession>, LaunchError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.launching = None;
        match result {
            Ok(session) => {
                self.make_room(window, cx);
                self.pane.update(cx, |pane, cx| {
                    pane.show(session, cx);
                    pane.set_draft("", window, cx);
                    pane.focus_composer(window, cx);
                });
                if std::mem::take(&mut self.open_after_launch) {
                    self.open_in_main(None, cx);
                }
            }
            Err(error) => {
                self.open_after_launch = false;
                self.error = Some(error.to_string());
            }
        }
        cx.notify();
    }

    /// The pane that shows the conversation.
    #[cfg(test)]
    pub(crate) fn pane_for_test(&self) -> &Entity<SessionPane> {
        &self.pane
    }

    /// Height asked of the window.
    #[cfg(test)]
    pub(crate) fn height_for_test(&self) -> f32 {
        self.height
    }

    /// Pretend the display is `height` px tall.
    #[cfg(test)]
    pub(crate) fn set_display_height_for_test(&mut self, height: f32) {
        self.display_height = Some(height);
    }

    /// The session picker is open.
    #[cfg(test)]
    pub(crate) fn picker_open_for_test(&self) -> bool {
        self.picker_open
    }

    /// Press the picker's layout toggle.
    #[cfg(test)]
    pub(crate) fn toggle_picker_layout_for_test(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout = self.picker_layout(cx).other();
        self.set_picker_layout(layout, window, cx);
    }

    /// Show or hide the session picker.
    #[cfg(test)]
    pub(crate) fn set_picker_for_test(&mut self, open: bool, cx: &mut Context<Self>) {
        self.picker_open = open;
        cx.notify();
    }

    /// Open or close the session list the way its button does, with the
    /// keyboard in its search field.
    #[cfg(test)]
    pub(crate) fn toggle_picker_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_picker(window, cx);
    }

    /// What was picked for the next quick chat.
    #[cfg(test)]
    pub(crate) fn picks_for_test(&self) -> Picks {
        self.picks.clone()
    }

    /// The folder chooser is open.
    #[cfg(test)]
    pub(crate) fn folder_open_for_test(&self) -> bool {
        self.folder_open
    }

    /// Last value of the opening animation.
    #[cfg(test)]
    pub(crate) fn opening_for_test(&self) -> f32 {
        self.opening.get()
    }

    /// The frame the ring last drew, `None` while it is not drawn.
    #[cfg(test)]
    pub(crate) fn ring_frame_for_test(&self) -> Option<EdgeFrame> {
        self.ring_frame.get()
    }

    /// Draw the light with `light` instead of the ring.
    #[cfg(test)]
    pub(crate) fn set_edge_light_for_test(&mut self, light: Box<dyn EdgeLight>) {
        self.light = Some(light);
    }

    #[cfg(test)]
    pub(crate) fn has_edge_light_for_test(&self) -> bool {
        self.light.is_some()
    }

    /// The model picker drawn in the panel.
    #[cfg(test)]
    pub(crate) fn models_for_test(&self) -> &Entity<ModelPicker> {
        &self.models
    }

    /// Drags started so far.
    #[cfg(test)]
    pub(crate) fn drags_for_test(&self) -> usize {
        self.drags
    }

    /// The colours the panel paints with.
    #[cfg(test)]
    pub(crate) fn hud_for_test(&self, cx: &App) -> Palette {
        self.hud(cx)
    }

    /// The session list's search field.
    #[cfg(test)]
    pub(crate) fn picker_query_for_test(&self) -> &Entity<InputState> {
        &self.query
    }

    /// The folder chooser's field.
    #[cfg(test)]
    pub(crate) fn folder_input_for_test(&self, cx: &App) -> Entity<InputState> {
        self.folders.read(cx).input_for_test().clone()
    }

    /// The model picked for the next quick chat.
    #[cfg(test)]
    pub(crate) fn model_for_test(&self) -> Option<&str> {
        self.picks.model.as_deref()
    }

    /// What the chrome is.
    #[cfg(test)]
    pub(crate) fn chrome_for_test(&self) -> Chrome {
        self.chrome
    }

    /// The error shown under the composer.
    #[cfg(test)]
    pub(crate) fn error_for_test(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The agent and the directory a quick chat starts with; `None` while no
    /// agent is connected.
    fn launch_parts(&self, cx: &App) -> Option<(String, Option<String>)> {
        let store = self.store.read(cx);
        let agent = store.primary_agent()?;
        let name = store
            .configs
            .iter()
            .find(|c| c.id == agent)
            .map_or(agent.as_str(), |c| c.name.as_str())
            .to_owned();
        let cwd = crate::util::workspace_name(&self.chosen_cwd(cx));
        Some((name, Some(cwd).filter(|c| !c.is_empty())))
    }

    /// The model the next quick chat starts on: the panel's choice, else the
    /// launch default.
    fn model_label(&self, cx: &App) -> String {
        let store = self.store.read(cx);
        store
            .primary_agent()
            .and_then(|agent| {
                store
                    .model_to_launch(&agent, self.picks.model.as_deref())
                    .map(|m| m.label.clone())
            })
            .unwrap_or_else(|| "Choose model".into())
    }

    /// Sessions the picker lists for the current query.
    pub(crate) fn picker_rows(&self, cx: &App) -> Vec<PickerRow> {
        let query = self.query.read(cx).value().trim().to_lowercase();
        picker_rows(self.store.read(cx), &query, cx)
    }

    /// How the picker lays its sessions out (a saved setting).
    pub(crate) fn picker_layout(&self, cx: &App) -> PickerLayout {
        self.store.read(cx).prefs.quick.picker_layout
    }

    /// Switch the picker between the list and the grid: saved with the quick
    /// panel settings, and the panel gets the height the layout needs.
    fn set_picker_layout(
        &mut self,
        layout: PickerLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.store
            .update(cx, |store, cx| store.set_quick_picker_layout(layout, cx));
        self.fit_height(window, cx);
        self.query.update(cx, |q, cx| q.focus(window, cx));
        cx.notify();
    }

    /// The panel's colours (design D15): dark whatever the app's theme is.
    fn hud(&self, cx: &App) -> Palette {
        hud_palette(palette(cx), self.chrome.glass)
    }

    /// A square icon button in the panel's colours.
    fn icon_button(
        id: &'static str,
        icon: IconName,
        tip: &'static str,
        p: &Palette,
    ) -> Stateful<Div> {
        let hover = p.card_hover;
        h_flex()
            .id(id)
            // Over a drag area: a press here is the button's (design D18).
            .occlude()
            .flex_none()
            .size(px(30.))
            .justify_center()
            .rounded(px(8.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .tooltip(move |window, cx| kit::wrap_tip(tip).build(window, cx))
            .child(kit::icon(icon, 16., p.muted))
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.hud(cx);
        let pane = self.pane.read(cx);
        let session = pane.session().is_some();
        let can_back = pane.can_go_back();
        let title = pane
            .view(cx)
            .map(|v| v.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "Quick chat".into());
        // No rule under it: the header is a small dim title row (design D15).
        // The header is a drag area (design D18); its controls occlude it.
        h_flex()
            .id("quick-header")
            .debug_selector(|| "quick-header".into())
            .flex_none()
            .h(px(HEADER_HEIGHT))
            .px_3()
            .gap_1()
            .items_center()
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| this.start_drag(window)),
            )
            .child(
                Self::icon_button(
                    "quick-new",
                    if can_back {
                        IconName::ArrowLeft
                    } else {
                        IconName::MessageSquarePlus
                    },
                    if can_back { "Back" } else { "New quick chat" },
                    &p,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if can_back {
                        this.pane.update(cx, |pane, cx| pane.go_back(cx));
                    } else {
                        this.new_chat(window, cx);
                    }
                })),
            )
            .child(
                // As wide as the title, so the rest of the header is free to
                // drag by.
                h_flex()
                    .id("quick-picker-toggle")
                    .debug_selector(|| "quick-title".into())
                    .occlude()
                    .min_w_0()
                    .h(px(30.))
                    .px_2()
                    .gap_1p5()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(p.card_hover))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(p.muted)
                            .child(title),
                    )
                    .child(kit::icon(IconName::ChevronDown, 14., p.muted))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_picker(window, cx))),
            )
            .child(
                div()
                    .debug_selector(|| "quick-drag".into())
                    .flex_1()
                    .h_full(),
            )
            .when(session, |el| {
                el.child(
                    Self::icon_button(
                        "quick-open-main",
                        IconName::ExternalLink,
                        "Open in main window",
                        &p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.open_in_main(None, cx))),
                )
            })
            .into_any_element()
    }

    /// The model picker in place of the conversation, while it is open
    /// (design D17): no dimming, card or shadow, on the panel's surface.
    fn render_model_sheet(&mut self, session: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.models.read(cx).is_open() {
            return None;
        }
        // Before the first prompt the chosen model is the panel's own; in a
        // session it is the session's.
        let selected = if session {
            None
        } else {
            self.picks.model.clone().map(Some)
        };
        let p = self.hud(cx);
        // What the sheet needs to show every row, read once it is laid out.
        let rows = self.models.read(cx).rows_scroll();
        let probe = Self::measure_with(&self.model_h, cx, move |whole| {
            ModelPicker::sheet_height(&rows, whole)
        })
        .into_any_element();
        let sheet = self.models.update(cx, |models, cx| {
            models.set_palette(Some(p));
            models.set_overrides(None, selected);
            models.card(PickerFrame::Sheet { probe }, cx)
        })?;
        Some(
            v_flex()
                .id("quick-models")
                .debug_selector(|| "quick-models".into())
                .flex_1()
                .min_h_0()
                .child(sheet)
                .into_any_element(),
        )
    }

    /// The folder chooser in place of the conversation, while it is open
    /// (design D17), its suggestions laid out under the field.
    fn render_folder_sheet(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.folder_open {
            return None;
        }
        let p = self.hud(cx);
        self.folders
            .update(cx, |folders, cx| folders.set_palette(Some(p), cx));
        // `flex_none` inside the scrolling box: the content keeps its natural
        // height, which is what the window's floor is measured from.
        Some(
            v_flex()
                .id("quick-folders")
                .debug_selector(|| "quick-folders".into())
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    v_flex()
                        .relative()
                        .flex_none()
                        .px_3()
                        .pt_3()
                        .pb_1()
                        .gap_2()
                        .child(Self::measure(&self.folder_h, cx))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text)
                                .child("Working folder"),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.muted)
                                .child("Pick or type the folder this chat runs in. Esc cancels."),
                        )
                        .child(self.folders.clone()),
                )
                .into_any_element(),
        )
    }

    /// A canvas that stores the height of the element it is placed in (which
    /// must be `relative`) and renders the panel again when it changed, so
    /// the window follows what is laid out (design D13).
    fn measure(cell: &Rc<Cell<f32>>, cx: &mut Context<Self>) -> impl Styled + IntoElement + use<> {
        Self::measure_with(cell, cx, |h| h)
    }

    /// [`Self::measure`], storing `f` of the height. `f` runs while painting,
    /// after the whole frame is laid out, so it may read other elements'
    /// bounds of this frame.
    fn measure_with<F: Fn(f32) -> f32 + 'static>(
        cell: &Rc<Cell<f32>>,
        cx: &mut Context<Self>,
        f: F,
    ) -> impl Styled + IntoElement + use<F> {
        let (cell, weak) = (cell.clone(), cx.entity().downgrade());
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let h = f(f32::from(bounds.size.height));
                if (cell.get() - h).abs() > 0.5 {
                    cell.set(h);
                    let weak = weak.clone();
                    window.on_next_frame(move |_, cx| {
                        let _ = weak.update(cx, |_, cx| cx.notify());
                    });
                }
            },
        )
        .absolute()
        .inset_0()
    }

    /// The session list, which takes the conversation's place (design D17):
    /// no card, border or shadow, rows straight on the panel's surface. In a
    /// new quick chat it lies above the bar. The search row stays in view and
    /// the rows scroll under it once the window cannot be taller.
    fn render_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = self.hud(cx);
        let layout = self.picker_layout(cx);
        let rows = self.picker_rows(cx);
        let empty = rows.is_empty();
        let cursor = self.picker_cursor.min(rows.len().saturating_sub(1));
        let (toggle_icon, toggle_tip) = match layout {
            PickerLayout::List => (IconName::LayoutGrid, "Show sessions as cards"),
            PickerLayout::Grid => (IconName::List, "Show sessions as a list"),
        };
        // Both parts carry their own padding, so what they measure is what
        // the list needs.
        let search = h_flex()
            .debug_selector(|| "quick-picker-search".into())
            .relative()
            .flex_none()
            .items_center()
            .gap_1()
            .px_2()
            .pb_1()
            .child(Self::measure(&self.list_search_h, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(kit::input_colours(&self.query, &p))
                    .child(
                        gpui_kit::component::input::Input::new(&self.query)
                            .appearance(false)
                            .text_color(p.text),
                    ),
            )
            .child(
                Self::icon_button("quick-picker-layout", toggle_icon, toggle_tip, &p)
                    .debug_selector(|| "quick-picker-layout".into())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_picker_layout(layout.other(), window, cx)
                    })),
            );
        let (list, scroll_id) = match layout {
            PickerLayout::List => (
                v_flex()
                    .gap_1()
                    .children(
                        rows.into_iter()
                            .enumerate()
                            .map(|(ix, row)| self.picker_list_row(ix, row, ix == cursor, &p, cx)),
                    )
                    .into_any_element(),
                "quick-picker-list",
            ),
            PickerLayout::Grid => {
                // Rows of two cards that share the width, so the columns hold
                // while the panel grows into its size.
                let mut cards = rows
                    .into_iter()
                    .enumerate()
                    .map(|(ix, row)| self.picker_card(ix, row, ix == cursor, &p, cx))
                    .collect::<Vec<_>>()
                    .into_iter();
                let mut grid_rows = Vec::new();
                while let Some(first) = cards.next() {
                    let second = cards
                        .next()
                        .unwrap_or_else(|| div().flex_1().min_w_0().into_any_element());
                    grid_rows.push(h_flex().gap(px(GRID_GAP)).child(first).child(second));
                }
                (
                    v_flex()
                        .gap(px(GRID_GAP))
                        .children(grid_rows)
                        .into_any_element(),
                    "quick-picker-cards",
                )
            }
        };
        // `flex_none` inside the scrolling box: the content keeps its natural
        // height, which is what the window's floor is measured from.
        let body = v_flex()
            .id(scroll_id)
            .debug_selector(move || scroll_id.into())
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .relative()
                    .flex_none()
                    .px_2()
                    .pb_2()
                    .child(Self::measure(&self.list_rows_h, cx))
                    .child(list)
                    .when(empty, |el| {
                        el.child(
                            div()
                                .px_2()
                                .py_2()
                                .text_sm()
                                .text_color(p.muted)
                                .child("No sessions match."),
                        )
                    }),
            );
        v_flex()
            .id("quick-picker")
            .debug_selector(|| "quick-picker".into())
            .flex_1()
            .min_h_0()
            // Before the search field, which would take the arrows for its caret.
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                if this.move_picker_cursor(false, cx) {
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                if this.move_picker_cursor(true, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(search)
            .child(body)
            .into_any_element()
    }

    /// A session's status chip in the picker: waiting or running.
    fn picker_tone(row: &PickerRow, p: &Palette) -> Option<(&'static str, Hsla)> {
        if row.waiting {
            Some(("Needs input", p.warn))
        } else if row.running {
            Some(("Running", p.accent))
        } else {
            None
        }
    }

    /// One line of the list layout. A session with progress has its row
    /// filled from the left as far as its share of done todos, and its count
    /// before the chip.
    fn picker_list_row(
        &self,
        ix: usize,
        row: PickerRow,
        selected: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *p;
        // Cmd/Ctrl+1… shows the row, as Spotlight does.
        let hint = if cx.platform_info().is_mac() {
            "⌘"
        } else {
            "Ctrl+"
        };
        let id = kit::keyed("quick-pick", format!("{}/{}", row.agent_id, row.id));
        let summary = row.summary.clone();
        let tone = Self::picker_tone(&row, &p);
        let progress = row.plan.as_ref().map(|d| {
            (
                progress_colour(&p, d.is_finished(), row.running),
                d.fraction(),
                d.done,
                d.total,
            )
        });
        h_flex()
            .id(id)
            .debug_selector(move || format!("quick-pick-row:{ix}"))
            .relative()
            .overflow_hidden()
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded(px(8.))
            .cursor_pointer()
            .hover(move |s| s.bg(p.card_hover))
            .when(selected, |el| el.bg(p.card_hover))
            // Under everything else of the row.
            .when_some(progress, |el, (colour, fraction, _, _)| {
                el.child(
                    div()
                        .debug_selector(move || format!("quick-pick-fill:{ix}"))
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(relative(fraction))
                        .bg(colour.opacity(PROGRESS_FILL_ALPHA)),
                )
            })
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .text_color(p.text)
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(p.muted)
                            .child(format!("{} · {}", row.agent_name, row.workspace)),
                    ),
            )
            .when_some(progress, |el, (_, _, done, total)| {
                el.child(
                    div()
                        .debug_selector(move || format!("quick-pick-count:{ix}:{done}/{total}"))
                        .flex_none()
                        .font_family(kit::MONO)
                        .text_xs()
                        .text_color(p.muted)
                        .child(format!("{done}/{total}")),
                )
            })
            .when_some(tone, |el, (label, color)| {
                el.child(kit::tone_chip(label, color))
            })
            .when(ix < 9, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(p.dim)
                        .child(format!("{hint}{}", ix + 1)),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| this.pick(&summary, window, cx)))
            .into_any_element()
    }

    /// One card of the grid layout: title and status, where and when, and
    /// for a session with todos a bar, the todo in progress, the next two and
    /// how many more there are.
    fn picker_card(
        &self,
        ix: usize,
        row: PickerRow,
        selected: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *p;
        let id = kit::keyed("quick-pick", format!("{}/{}", row.agent_id, row.id));
        let summary = row.summary.clone();
        let tone = Self::picker_tone(&row, &p);
        let when = crate::util::short_time(&row.summary.updated_at);
        let meta = if when.is_empty() {
            format!("{} · {}", row.agent_name, row.workspace)
        } else {
            format!("{} · {} · {when}", row.agent_name, row.workspace)
        };
        let progress = row.plan.as_ref().map(|d| {
            let colour = progress_colour(&p, d.is_finished(), row.running);
            let shown: Vec<_> = match d.current() {
                Some(current) => std::iter::once(current).chain(d.upcoming(2)).collect(),
                None => d.upcoming(3),
            };
            let hidden = d.hidden(shown.len());
            let (done, total) = (d.done, d.total);
            let bar = h_flex()
                .debug_selector(move || format!("quick-card-bar:{ix}:{done}/{total}"))
                .h(px(14.))
                .gap_2()
                .items_center()
                .child(
                    div().flex_1().h(px(4.)).rounded_full().bg(p.chip).child(
                        div()
                            .debug_selector(move || format!("quick-card-fill:{ix}"))
                            .h_full()
                            .w(relative(d.fraction()))
                            .rounded_full()
                            .bg(colour),
                    ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(kit::MONO)
                        .text_xs()
                        .text_color(p.muted)
                        .child(format!("{done}/{total}")),
                );
            let lines: Vec<AnyElement> = shown
                .into_iter()
                .map(|e| {
                    let (icon, colour) = kit::plan_entry_mark(&e.status, &p);
                    let content = e.content.clone();
                    let text = if e.status == "in_progress" {
                        p.text
                    } else {
                        p.muted
                    };
                    h_flex()
                        .debug_selector(move || format!("quick-card-line:{ix}:{content}"))
                        .h(px(16.))
                        .gap_1p5()
                        .items_center()
                        .child(div().flex_none().child(kit::icon(icon, 11., colour)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(text)
                                .child(e.content.clone()),
                        )
                        .into_any_element()
                })
                .collect();
            let more = (hidden > 0).then(|| {
                div()
                    .debug_selector(move || format!("quick-card-more:{ix}:{hidden}"))
                    .h(px(16.))
                    .text_xs()
                    .text_color(p.dim)
                    .child(format!("+{hidden} more"))
            });
            v_flex()
                .gap(px(3.))
                .child(bar)
                .children(lines)
                .children(more)
                .into_any_element()
        });
        v_flex()
            .id(id)
            .debug_selector(move || format!("quick-pick-card:{ix}"))
            .flex_1()
            .min_w_0()
            .min_h(px(GRID_CARD))
            .px_2()
            .py(px(6.))
            .gap(px(3.))
            .rounded(px(10.))
            .bg(p.card)
            .cursor_pointer()
            .hover(move |s| s.bg(p.card_hover))
            .when(selected, |el| el.bg(p.card_hover))
            .child(
                h_flex()
                    .h(px(20.))
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(p.text)
                            .child(row.title.clone()),
                    )
                    .when_some(tone, |el, (label, color)| {
                        el.child(kit::tone_chip(label, color))
                    }),
            )
            .child(
                div()
                    .h(px(16.))
                    .truncate()
                    .text_xs()
                    .text_color(p.muted)
                    .child(meta),
            )
            .children(progress)
            .on_click(cx.listener(move |this, _, window, cx| this.pick(&summary, window, cx)))
            .into_any_element()
    }

    /// What the panel shows before the first prompt of a quick chat.
    /// A new quick chat: its bar, with the session `list` above it when open.
    fn render_empty(&mut self, list: Option<AnyElement>, cx: &mut Context<Self>) -> AnyElement {
        let p = self.hud(cx);
        // The agent and folder, then the model as a button.
        let summary = match self.launch_parts(cx) {
            None => div()
                .flex_none()
                .text_xs()
                .text_color(p.muted)
                .child("No agent connected")
                .into_any_element(),
            Some((agent, cwd)) => {
                let model = self.model_label(cx);
                // The agent is named in the bar only when there is a choice.
                let several = self.store.read(cx).configs.len() > 1;
                let folder_tip = format!(
                    "{agent} · {}: change the folder (Cmd/Ctrl+O)",
                    self.chosen_cwd(cx)
                );
                let reasoning = self.reasoning_now(cx);
                let auto = reasoning == "auto";
                let reasoning_label = LAUNCH_REASONING
                    .iter()
                    .find(|(id, ..)| *id == reasoning)
                    .map_or(reasoning.as_str(), |(_, label, _)| *label)
                    .to_string();
                let reasoning_tip = format!("Reasoning effort: {reasoning_label} (Cmd/Ctrl+T)");
                // One shape for the chips, all the height of the model chip.
                let chip = |id: &'static str| {
                    h_flex()
                        .id(id)
                        .debug_selector(move || id.into())
                        .occlude()
                        .h(px(26.))
                        .gap_1()
                        .px_2()
                        .rounded(px(13.))
                        .bg(p.chip)
                        .cursor_pointer()
                        .hover(move |s| s.bg(p.card_hover))
                };
                h_flex()
                    .flex_none()
                    .gap_1p5()
                    .items_center()
                    .text_xs()
                    .text_color(p.muted)
                    // The values read in `text`, their icons in `muted`: what
                    // the chat runs with must read first (design D11).
                    .when(several, |el| {
                        el.child(
                            div()
                                .max_w(px(70.))
                                .truncate()
                                .text_color(p.text)
                                .child(agent.clone()),
                        )
                    })
                    .child(
                        chip("quick-folder")
                            .tooltip(move |window, cx| {
                                kit::wrap_tip(folder_tip.clone()).build(window, cx)
                            })
                            .child(kit::icon(IconName::Folder, 12., p.muted))
                            .child(
                                div()
                                    .max_w(px(100.))
                                    .truncate()
                                    .text_color(p.text)
                                    .child(cwd.unwrap_or_else(|| "Choose folder".into())),
                            )
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_folder_chooser(window, cx)
                            })),
                    )
                    .child(
                        chip("quick-model")
                            .tooltip(|window, cx| {
                                kit::wrap_tip("Model for this chat (Cmd/Ctrl+M)").build(window, cx)
                            })
                            .child(
                                div()
                                    .max_w(px(130.))
                                    .truncate()
                                    .text_color(p.text)
                                    .child(model),
                            )
                            .child(kit::icon(IconName::ChevronDown, 12., p.muted))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_model_picker(window, cx)
                            })),
                    )
                    .child(
                        chip("quick-reasoning")
                            .tooltip(move |window, cx| {
                                kit::wrap_tip(reasoning_tip.clone()).build(window, cx)
                            })
                            .child(kit::icon(
                                IconName::Brain,
                                13.,
                                if auto { p.muted } else { p.text },
                            ))
                            // The level shows once it is not the default.
                            .when(!auto, |el| {
                                el.child(div().text_color(p.text).child(reasoning_label.clone()))
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_reasoning(cx))),
                    )
                    .into_any_element()
            }
        };
        let launching = self.launching.is_some();
        let error = self.error.clone();
        let input = div()
            .debug_selector(|| "quick-composer".into())
            // Pressing in the text selects it; it does not drag (design D18).
            .occlude()
            .flex_1()
            .min_w_0()
            .capture_action(cx.listener(|this, a: &Enter, window, cx| {
                // Enter starts the chat, Shift+Enter breaks the line, and
                // Cmd/Ctrl+Enter starts it in the main window.
                if a.secondary {
                    cx.stop_propagation();
                    this.submit_and_open(window, cx);
                } else if !a.shift {
                    cx.stop_propagation();
                    this.submit(window, cx);
                }
            }))
            // Placeholder, selection and caret in the panel's colours.
            .child(kit::input_colours(self.pane.read(cx).prompt_input(), &p))
            .child(
                Textarea::new(self.pane.read(cx).prompt_input())
                    .appearance(false)
                    // The kit's input paints in the app theme's foreground
                    // (gpui-kit 0.11), which is dark in a light app theme.
                    .text_color(p.text)
                    .text_size(px(BAR_TEXT))
                    .line_height(px(BAR_LINE)),
            );
        // The bar is the whole panel: nothing is drawn around the input, which
        // sits straight on the panel's surface (design D15).
        // Around its controls the bar is a drag area (design D18).
        let bar = h_flex()
            .id("quick-bar")
            .debug_selector(|| "quick-bar".into())
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| this.start_drag(window)),
            )
            .w_full()
            .min_h(px(BAR_HEIGHT - 2.))
            .items_center()
            .gap_2()
            .pl_4()
            .pr_3()
            .font_family(kit::SYSTEM_UI)
            .child(input)
            .when(launching, |el| {
                el.child(kit::spinner("quick-launch", 16., p.muted))
            })
            .child(summary)
            .child(
                Self::icon_button(
                    "quick-picker-toggle",
                    IconName::MessagesSquare,
                    "Sessions",
                    &p,
                )
                .on_click(cx.listener(|this, _, window, cx| this.toggle_picker(window, cx))),
            );
        // What the bar and the error above it need: the window follows it.
        let measure = Self::measure(&self.bar_h, cx);
        // The bar stays at the bottom edge, which is the one that does not
        // move when the window grows (design D13).
        v_flex()
            .flex_1()
            .min_h_0()
            .justify_end()
            .children(list)
            .child(
                v_flex()
                    .relative()
                    .flex_none()
                    .w_full()
                    .child(measure)
                    .when_some(error, |el, error| {
                        el.child(
                            h_flex()
                                .debug_selector(|| "quick-error".into())
                                .gap_1p5()
                                .px_4()
                                .pt_2p5()
                                .text_xs()
                                .text_color(p.danger)
                                .child(kit::icon(IconName::CircleAlert, 13., p.danger))
                                .child(div().flex_1().min_w_0().child(error)),
                        )
                    })
                    .child(bar),
            )
            .into_any_element()
    }
}

impl QuickPanel {
    /// Read the shown session into the activity tracker (design D14): the one
    /// place that says whether the agent is working for this session.
    fn track_activity(&mut self, cx: &App) -> Activity {
        let now = cx.background_executor().now();
        let observation = {
            let pane = self.pane.read(cx);
            match (pane.key(cx), pane.view(cx)) {
                (Some(key), Some(view)) => {
                    let pending = self.store.read(cx).requests.iter().any(|r| {
                        r.is_pending()
                            && r.agent_id == key.agent.as_str()
                            && r.session_id.as_deref() == Some(key.session.as_str())
                    });
                    activity::observe(view, &key, pending)
                }
                _ => Observation::default(),
            }
        };
        self.tracker.observe(now, observation)
    }

    /// Bring the light along the edge up to date (design D14), and tell a
    /// renderer outside GPUI what changed. Returns what the ring draws from.
    fn update_edge(&mut self, p: &Palette, size: (f32, f32), cx: &mut Context<Self>) -> EdgeState {
        let now = cx.background_executor().now();
        let activity = self.track_activity(cx);
        if activity != self.last_activity {
            self.last_activity = activity;
            self.low_power = chrome::low_power();
        }
        let moving = !cx.reduce_motion() && !self.low_power;
        let rate = self.tracker.rate(now);
        let update = self.edge.update(now, activity, rate, moving);
        if update.settle {
            self.tracker.settle();
        }
        let colours = edge_colours(p);
        if let Some(light) = &mut self.light {
            let state = self.edge.state();
            let result = (|| {
                if self.light_size != Some(size) {
                    light.resize(size.0, size.1)?;
                    self.light_size = Some(size);
                }
                if update.changed || self.light_colours != Some(colours) {
                    light.show(state, &colours)?;
                    self.light_colours = Some(colours);
                }
                Ok::<(), edge::EdgeError>(())
            })();
            if let Err(err) = result {
                // The ring takes over for the rest of this panel's life.
                tracing::warn!(%err, "edge light failed; drawing the ring");
                self.light = None;
            }
        }
        self.schedule_wake(now, cx);
        *self.edge.state()
    }

    /// Render again when the edge next changes by itself: the light appearing
    /// after its delay, an end finishing, a fade ending, the lap time to check.
    fn schedule_wake(&mut self, now: std::time::Instant, cx: &mut Context<Self>) {
        let Some(at) = self.edge.next_wake(now) else {
            self.wake = None;
            return;
        };
        if self.wake.as_ref().is_some_and(|(when, _)| *when == at) {
            return;
        }
        let delay = Duration::from_secs_f64((at - self.edge.secs(now)).max(0.));
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.wake = None;
                cx.notify();
            });
        });
        self.wake = Some((at, task));
    }
}

/// The light's colours from the HUD palette.
fn edge_colours(p: &Palette) -> EdgeColours {
    let rgba = |c: Hsla| {
        let c = Rgba::from(c);
        [c.r, c.g, c.b, c.a]
    };
    EdgeColours {
        accent: rgba(p.accent),
        warn: rgba(p.warn),
        danger: rgba(p.danger),
    }
}

/// Strokes along `outline`, `width` wide, `alpha` strong: a closed loop, or
/// `share` of it from `from`.
fn stroke(
    window: &mut Window,
    origin: Point<Pixels>,
    outline: &Outline,
    (from, share): (f32, f32),
    width: f32,
    colour: [f32; 4],
    alpha: f32,
) {
    if alpha <= 0.002 || share <= 0. {
        return;
    }
    let closed = share >= 1.;
    let points = outline.points(from, share.min(1.), 3.);
    let Some(first) = points.first() else {
        return;
    };
    let at = |(x, y): (f32, f32)| point(origin.x + px(x), origin.y + px(y));
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(at(*first));
    for p in &points[1..] {
        path.line_to(at(*p));
    }
    if closed {
        path.close();
    }
    if let Ok(path) = path.build() {
        let colour = Rgba {
            r: colour[0],
            g: colour[1],
            b: colour[2],
            a: (colour[3] * alpha).clamp(0., 1.),
        };
        window.paint_path(path, Hsla::from(colour));
    }
}

/// The light along the edge, drawn by GPUI (design D14): the line on the
/// whole edge and its inward halo, and the travelling highlight with its
/// tail and its glow. Everything stays inside the panel's rounded shape.
fn paint_ring(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    radius: f32,
    frame: &EdgeFrame,
    colours: &EdgeColours,
) {
    let outline = Outline::new(
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
        radius,
    );
    let origin = bounds.origin;
    let line_colour = colours.of(frame.line_role);
    let accent = colours.accent;
    let segment = (frame.head - frame.tail, frame.tail);
    // The glow: strokes of growing width, inset by half of each, so they
    // add up to a falloff from the edge inward.
    for (share, width) in [(0.5, 4.), (0.3, 9.), (0.2, MOTION.glow_width)] {
        let inner = outline.inset(width / 2.);
        let glow = MOTION.glow_alpha * share;
        stroke(
            window,
            origin,
            &inner,
            (0., 1.),
            width,
            line_colour,
            glow * frame.halo,
        );
        let strength = glow * frame.glow * frame.highlight * (1. + frame.flare);
        stroke(window, origin, &inner, segment, width, accent, strength);
    }
    let line = outline.inset(MOTION.line_width / 2.);
    stroke(
        window,
        origin,
        &line,
        (0., 1.),
        MOTION.line_width,
        line_colour,
        frame.line,
    );
    // The highlight: its tail in three pieces, fainter away from the head.
    let width = MOTION.line_width * (1. + frame.flare);
    let piece = frame.tail / 3.;
    for (i, strength) in [0.25, 0.55, 1.].into_iter().enumerate() {
        let from = frame.head - frame.tail + piece * i as f32;
        stroke(
            window,
            origin,
            &line,
            (from, piece),
            width,
            accent,
            strength * frame.highlight,
        );
    }
}

impl Render for QuickPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.hud(cx);
        let s = self.chrome.surfaces(&p);
        let session = self.pane.read(cx).session().is_some();
        self.pane.update(cx, |pane, cx| {
            // The user's accent can change while the panel is open; this only
            // notifies the pane when the colours differ.
            pane.set_palette(Some(p), cx);
            // Alone, the input is a bar of one line.
            pane.set_composer_bar(!session, cx);
        });
        let viewport = window.viewport_size();
        let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
        let edge = self.update_edge(&p, (width, height), cx);
        // Resize after this frame's layout, at most once per frame.
        if self.wanted_height(window, cx).is_some() {
            let this = cx.entity().downgrade();
            window.on_next_frame(move |window, cx| {
                let _ = this.update(cx, |this, cx| this.fit_height(window, cx));
            });
        }
        // The final size is `viewport`; the panel starts a little smaller.
        let opening = self.opening.clone();
        // Where the material is a view of ours, the window fades as a whole
        // and the material is scaled with the content; the step is taken
        // while painting, after the animation set this frame's value.
        let native = self.chrome.native_motion;
        let step = native.then(|| {
            let (opening, stepped) = (self.opening.clone(), self.stepped.clone());
            canvas(
                move |_, window, _| {
                    let t = opening.get();
                    let now = (open_opacity(t), open_scale(t));
                    if stepped.replace(Some(now)) != Some(now) {
                        chrome::open_step(window, now.0, now.1);
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_0()
        });
        // The light along the edge, drawn here when no renderer outside GPUI
        // draws it.
        let ring_frame = self.ring_frame.clone();
        let t = self.edge.secs(cx.background_executor().now());
        let ring = (self.light.is_none() && edge.visible(t)).then(|| {
            let (origin, colours) = (self.edge.origin(), edge_colours(&p));
            let radius = f32::from(self.chrome.radius);
            canvas(
                |_, _, _| {},
                move |bounds, _, window, cx| {
                    let t = edge::secs_since(origin, cx.background_executor().now());
                    let frame = edge::frame_at(&edge, t);
                    ring_frame.set(Some(frame));
                    paint_ring(window, bounds, radius, &frame, &colours);
                    if edge.animating(t) {
                        window.request_animation_frame();
                    }
                },
            )
            .absolute()
            .inset_0()
        });
        if ring.is_none() {
            self.ring_frame.set(None);
        }
        // The session list, the model picker and the folder chooser take the
        // conversation's place (design D17); the pane is kept, so the
        // conversation returns where it was scrolled to. In a new quick chat
        // they lie above the bar. At most one is open.
        let list = match self.render_folder_sheet(cx) {
            Some(sheet) => Some(sheet),
            None => match self.render_model_sheet(session, cx) {
                Some(sheet) => Some(sheet),
                None => self.picker_open.then(|| self.render_picker(cx)),
            },
        };
        // A new quick chat is only its bar.
        let header = session.then(|| self.render_header(cx));
        let body = if !session {
            self.render_empty(list, cx)
        } else if let Some(list) = list {
            list
        } else {
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(self.pane.clone())
                .into_any_element()
        };
        v_flex()
            .id("quick-panel")
            .absolute()
            .top_0()
            .bottom_0()
            .left_0()
            .right_0()
            .overflow_hidden()
            .rounded(self.chrome.radius)
            .border_1()
            .border_color(s.border)
            .bg(s.bg)
            .text_color(p.text)
            // Before the composer, so it cannot swallow Esc.
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    cx.stop_propagation();
                    if this
                        .pane
                        .update(cx, |pane, cx| pane.close_slash_list(window, cx))
                    {
                        // Only the composer's command list closes.
                        cx.notify();
                    } else if this.close_folder_chooser(window, cx) {
                        // The composer gets the focus back.
                    } else if this.models.update(cx, |models, cx| models.close(cx)) {
                        // The composer gets the focus back when it closes.
                    } else if std::mem::take(&mut this.picker_open) {
                        this.pane
                            .update(cx, |pane, cx| pane.focus_composer(window, cx));
                        cx.notify();
                    } else {
                        this.dismiss(window, cx);
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &OpenModelPicker, window, cx| {
                this.open_model_picker(window, cx)
            }))
            .on_action(cx.listener(|this, _: &CycleReasoning, _, cx| this.cycle_reasoning(cx)))
            .on_action(cx.listener(|this, _: &ChooseFolder, window, cx| {
                this.open_folder_chooser(window, cx)
            }))
            // Cmd/Ctrl+1… shows the nth session of the open list.
            .on_action(cx.listener(|this, a: &OpenSidebarSession, window, cx| {
                this.pick_nth(a.index, window, cx);
            }))
            .children(header)
            .child(body)
            .children(ring)
            .children(step)
            .with_animation(
                "quick-open",
                // Linear: the curves below shape the time themselves.
                Animation::new(Duration::from_millis(OPEN_MS)),
                move |panel, delta| {
                    opening.set(delta);
                    // GPUI has no transform for a `div`: the scale is an inset.
                    let inset = (1. - open_scale(delta)) / 2.;
                    let (x, y) = (width * inset, height * inset);
                    panel
                        .top(px(y))
                        .bottom(px(y))
                        .left(px(x))
                        .right(px(x))
                        .opacity(if native { 1. } else { open_opacity(delta) })
                },
            )
    }
}

/// Opacity of the panel `t` (0 to 1) into its opening: eased at both ends,
/// and full when the panel arrives at its size.
pub(super) fn open_opacity(t: f32) -> f32 {
    let u = (t / OPEN_ARRIVE).clamp(0., 1.);
    u * u * (3. - 2. * u)
}

/// Scale of the panel `t` (0 to 1) into its opening (design D14). It gathers
/// speed on the way to its full size, reaches it at [`OPEN_ARRIVE`], springs
/// back by [`OPEN_BUMP`] and settles: the bump of something arriving. It never
/// passes 1, because the window is the panel's final size and would clip it.
pub(super) fn open_scale(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    if t < OPEN_ARRIVE {
        let u = t / OPEN_ARRIVE;
        OPEN_SCALE_FROM + (1. - OPEN_SCALE_FROM) * u.powf(1.6)
    } else {
        let v = (t - OPEN_ARRIVE) / (1. - OPEN_ARRIVE);
        // Zero at both ends: quick off the arrival, slow into rest.
        1. - OPEN_BUMP * BUMP_NORM * (std::f32::consts::PI * v).sin() * (1. - v)
    }
}

/// The window height for a transcript `overflow` px taller than its
/// viewport (design D13).
///
/// It is `current` plus the overflow, in steps of [`STEP`], capped at
/// [`MAX_HEIGHT`] and at [`DISPLAY_SHARE`] of the display, and raised to
/// `floor` for what is open over the conversation. A conversation is at least
/// [`MIN_HEIGHT`] tall and never shrinks on its own; a new quick chat is its
/// bar, [`BAR_HEIGHT`], and follows the floor both ways, exactly (no steps),
/// so the bar's content stays centred in the window.
pub(super) fn fitted_height(
    current: f32,
    overflow: f32,
    floor: f32,
    display: f32,
    shown: bool,
) -> f32 {
    let cap = (display * DISPLAY_SHARE).clamp(MIN_HEIGHT, MAX_HEIGHT);
    // An overlay is never clipped, even on a small display.
    let floor = floor.min(display * OVERLAY_SHARE);
    let limit = cap.max(floor);
    // What this state is at least tall, and what steps count from.
    let least = if shown { MIN_HEIGHT } else { BAR_HEIGHT };
    let wanted = if shown {
        current + overflow.max(0.)
    } else {
        0.
    }
    .max(least)
    .max(floor);
    if shown && wanted <= current {
        // Nothing to grow into: keep the height (only a smaller display
        // takes room back).
        return current.min(limit);
    }
    if !shown {
        // The bar sits on the bottom edge: any height beyond what it needs
        // would be empty space above it, so its height is not stepped.
        return wanted.ceil().min(limit);
    }
    let stepped = least + ((wanted - least) / STEP).ceil().max(0.) * STEP;
    stepped.min(limit)
}

/// One line of the session picker.
#[derive(Clone)]
pub(crate) struct PickerRow {
    pub(crate) agent_id: String,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) agent_name: String,
    pub(crate) workspace: String,
    /// The session waits for the user.
    pub(crate) waiting: bool,
    pub(crate) running: bool,
    /// Progress of the session's todos, if it has any.
    pub(crate) plan: Option<PlanDigest>,
    summary: SessionSummary,
}

/// Height the open session picker needs besides the conversation: its
/// chrome and `rows` sessions, in a column (list) or two (grid). Always room
/// for one, for the empty message.
pub(super) fn picker_height(layout: PickerLayout, rows: usize) -> f32 {
    let rows = rows.max(1);
    match layout {
        PickerLayout::List => PICKER_CHROME + rows as f32 * PICKER_ROW,
        PickerLayout::Grid => {
            PICKER_CHROME + rows.div_ceil(GRID_COLUMNS) as f32 * (GRID_CARD + GRID_GAP)
        }
    }
}

/// Sessions matching `query` (lower case; empty matches all): those waiting
/// for the user first, then running ones, then the rest, each group newest
/// first.
pub(crate) fn picker_rows(store: &AgentsStore, query: &str, cx: &App) -> Vec<PickerRow> {
    let waiting: HashSet<(&str, &str)> = store
        .requests
        .iter()
        .filter(|r| r.is_pending() && !r.offline)
        .filter_map(|r| Some((r.agent_id.as_str(), r.session_id.as_deref()?)))
        .collect();
    let agent_name = |id: &str| {
        store
            .configs
            .iter()
            .find(|c| c.id == id)
            .map_or_else(|| id.to_owned(), |c| c.name.clone())
    };
    let mut rows: Vec<PickerRow> = store
        .all_sessions()
        .into_iter()
        .filter(|s| s.root && !s.archived)
        .filter_map(|s| {
            let name = agent_name(&s.agent_id);
            if !crate::ui::matches_query(s, &name, query) {
                return None;
            }
            Some(PickerRow {
                agent_id: s.agent_id.clone(),
                id: s.id.clone(),
                title: s.display_title(),
                workspace: crate::util::workspace_name(&s.cwd),
                waiting: waiting.contains(&(s.agent_id.as_str(), s.id.as_str())),
                running: s.running(),
                plan: store.plan_digest(s, cx),
                agent_name: name,
                summary: s.clone(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let rank = |r: &PickerRow| {
            if r.waiting {
                0
            } else if r.running {
                1
            } else {
                2
            }
        };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.summary.updated_at.cmp(&a.summary.updated_at))
    });
    rows.truncate(PICKER_ROWS);
    rows
}
