//! Window-wide smooth wheel scrolling.
//!
//! A mouse-wheel notch reaches GPUI as `ScrollDelta::Lines`, and every scroll
//! container applies the whole jump in one frame. Trackpads send precise pixel
//! deltas and already feel smooth; GPUI only synthesizes momentum for touch.
//!
//! This swallows notch events in the capture phase (before any container sees
//! them), converts them to a pixel target, and each frame re-dispatches a
//! fraction of the remaining distance as a precise `ScrollDelta::Pixels` event
//! within the originating scroll region. Crossing a region boundary cancels
//! pending motion; containers never inherit another region's wheel gesture.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::*;

// The executor's clock type, as gpui's scheduler defines it per target.
#[cfg(not(target_family = "wasm"))]
use std::time::Instant;
#[cfg(target_family = "wasm")]
use web_time::Instant;

/// Longest step the animation waits for; a stalled frame doesn't teleport.
const MAX_DT: Duration = Duration::from_millis(50);

/// User-facing smoothing presets: (pref id, label, ease-out time constant).
/// `TAU` is the time to cover ~63% of the remaining distance; `None` turns
/// the animation off and lets wheel notches jump as GPUI does natively.
pub const PRESETS: [(&str, &str, Option<Duration>); 5] = [
    ("off", "Off", None),
    ("snappy", "Snappy", Some(Duration::from_millis(50))),
    ("normal", "Normal", Some(Duration::from_millis(90))),
    ("smooth", "Smooth", Some(Duration::from_millis(150))),
    ("glide", "Glide", Some(Duration::from_millis(240))),
];

pub const DEFAULT_PRESET: &str = "normal";

/// Time constant for a preset id; unknown ids fall back to the default.
pub fn tau_for(preset: &str) -> Option<Duration> {
    let find = |id: &str| {
        PRESETS
            .iter()
            .find(|(p, _, _)| *p == id)
            .map(|(_, _, tau)| *tau)
    };
    find(preset).unwrap_or_else(|| find(DEFAULT_PRESET).flatten())
}

/// Shape of the motion, independent of its duration.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Curve {
    /// Full speed at once, then slowing: each frame covers a fixed fraction of
    /// what's left (exponential decay).
    #[default]
    EaseOut,
    /// Browser-like: speed ramps up briefly, peaks, then tails off. A
    /// critically damped spring toward the target, so a notch landing
    /// mid-motion keeps the current velocity instead of restarting.
    EaseInOut,
}

/// Curve choices: (pref id, label).
pub const CURVES: [(&str, &str, Curve); 2] = [
    ("ease_out", "Ease out", Curve::EaseOut),
    ("ease_in_out", "Ease in-out", Curve::EaseInOut),
];

pub const DEFAULT_CURVE: &str = "ease_out";

pub fn curve_for(id: &str) -> Curve {
    CURVES
        .iter()
        .find(|(c, _, _)| *c == id)
        .map(|(_, _, curve)| *curve)
        .unwrap_or_default()
}

/// Browser-like wheel inertia, independent of duration and curve:
/// - acceleration: notches arriving in quick succession scroll further each;
/// - momentum: after a fast spin stops, the view coasts on and slows down.
///
/// The coast is the distance a friction-decayed motion would cover
/// (`speed * carry * friction`), appended to the target while the animation
/// is stretched by the same ratio, so speed carries on without a jump.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Inertia {
    /// Extra distance per consecutive fast notch (`1 + streak * accel`).
    accel: f32,
    /// Cap on the acceleration multiplier.
    accel_max: f32,
    /// Fraction of the spin speed carried into the coast.
    carry: f32,
    /// Friction time constant of the coast.
    friction: Duration,
}

/// Inertia choices: (pref id, label, params); `None` = no inertia.
pub const INERTIAS: [(&str, &str, Option<Inertia>); 3] = [
    ("off", "Off", None),
    (
        "light",
        "Light",
        Some(Inertia {
            accel: 0.15,
            accel_max: 2.0,
            carry: 0.35,
            friction: Duration::from_millis(220),
        }),
    ),
    (
        "strong",
        "Strong",
        Some(Inertia {
            accel: 0.3,
            accel_max: 4.0,
            carry: 0.6,
            friction: Duration::from_millis(420),
        }),
    ),
];

pub const DEFAULT_INERTIA: &str = "off";

pub fn inertia_for(id: &str) -> Option<Inertia> {
    INERTIAS
        .iter()
        .find(|(i, _, _)| *i == id)
        .and_then(|(_, _, inertia)| *inertia)
}

/// Notches closer together than this count as one continuous spin; a longer
/// pause means the wheel was released.
const SPIN_GAP: Duration = Duration::from_millis(140);
/// Spin speed (px/s) needed before the view coasts on release.
const COAST_MIN_SPEED: f32 = 900.;
/// Longest a coast may stretch the motion, as a multiple of its duration.
const MAX_STRETCH: f32 = 6.;

/// Wheel-spin tracking for inertia, per axis.
#[derive(Default)]
struct Spin {
    last_notch: Option<Instant>,
    /// Consecutive fast notches in the same direction.
    streak: u32,
    /// Smoothed wheel speed in px/s.
    speed: Point<f32>,
}

#[derive(Default)]
struct State {
    /// Distance still to be scrolled, in pixels.
    remaining: Point<f32>,
    /// Current velocity in px/s (ease in-out only).
    velocity: Point<f32>,
    /// Current ease-out time constant; `None` disables smoothing.
    tau: Option<Duration>,
    curve: Curve,
    inertia: Option<Inertia>,
    spin: Spin,
    /// Duration multiplier while coasting after a fast spin; 1 otherwise.
    stretch: f32,
    animating: bool,
    /// Set while a synthetic event is being dispatched, so the capture
    /// listener lets it through.
    injecting: bool,
    last_tick: Option<Instant>,
    owner: Option<SharedString>,
    regions: Vec<(SharedString, Bounds<Pixels>)>,
    /// `scroll.gesture` span from the first notch until the motion settles
    /// or is cancelled; step events and transcript frame samples attach to it.
    gesture: Option<tracing::Span>,
    /// Injected distance (px, + = toward the start) not yet consumed by a
    /// transcript frame sample.
    pending_input: f32,
}

impl State {
    fn region_at(&self, position: Point<Pixels>) -> Option<SharedString> {
        self.regions
            .iter()
            .rev()
            .find(|(_, bounds)| bounds.contains(&position))
            .map(|(id, _)| id.clone())
    }

    fn reset_motion(&mut self) {
        if let Some(span) = self.gesture.take() {
            tracing::debug!(parent: &span, remaining_y = self.remaining.y, "scroll motion cancelled");
        }
        self.remaining = Point::default();
        self.velocity = Point::default();
        self.spin = Spin::default();
        self.stretch = 1.;
        self.owner = None;
    }

    fn retain_owner(&mut self, position: Point<Pixels>) {
        if self.owner.is_some() && self.owner != self.region_at(position) {
            self.reset_motion();
        }
    }
}

/// Geometry-only wrapper: delegates the child's layout unchanged and records
/// the clipped viewport, not a canvas inside the moving scroll content.
struct ScrollRegion {
    state: Rc<RefCell<State>>,
    id: SharedString,
    element: AnyElement,
}

impl Element for ScrollRegion {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.element.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let bounds = bounds.intersect(&window.content_mask().bounds);
        if bounds.size.width > px(0.) && bounds.size.height > px(0.) {
            // Parents register before children; deferred overlays register last.
            self.state
                .borrow_mut()
                .regions
                .push((self.id.clone(), bounds));
        }
        self.element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.element.paint(window, cx);
    }
}

#[derive(Clone)]
pub struct SmoothScroll(Rc<RefCell<State>>);

impl Default for SmoothScroll {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(State {
            tau: tau_for(DEFAULT_PRESET),
            stretch: 1.,
            ..Default::default()
        })))
    }
}

impl SmoothScroll {
    /// Apply the duration preset and curve (from prefs); takes effect on the
    /// next wheel notch.
    pub fn set_preset(&self, preset: &str, curve: &str, inertia: &str) {
        let tau = tau_for(preset);
        let curve = curve_for(curve);
        let inertia = inertia_for(inertia);
        let mut s = self.0.borrow_mut();
        if s.inertia != inertia {
            s.inertia = inertia;
            s.spin = Spin::default();
            s.stretch = 1.;
        }
        if s.tau != tau {
            s.tau = tau;
            if tau.is_none() {
                s.reset_motion();
            }
        }
        if s.curve != curve {
            s.curve = curve;
            s.velocity = Point::default();
        }
    }

    /// Injected distance since the last call, and the current gesture span,
    /// for the transcript's per-frame telemetry sample.
    pub fn take_frame_input(&self) -> (f32, Option<tracing::Span>) {
        let mut s = self.0.borrow_mut();
        (std::mem::take(&mut s.pending_input), s.gesture.clone())
    }

    /// Register resolved geometry without adding a layout container. Only
    /// visible regions are retained, and the allocation is reused each frame.
    pub fn region(
        &self,
        id: impl Into<SharedString>,
        element: impl IntoElement,
    ) -> impl IntoElement {
        ScrollRegion {
            state: self.0.clone(),
            id: id.into(),
            element: element.into_any_element(),
        }
    }

    /// Place first in the root: starts the region list during prepaint and
    /// installs capture listeners before any scroll container's listeners.
    pub fn element(&self) -> impl IntoElement {
        let this = self.clone();
        let state = self.0.clone();
        canvas(
            move |_, _, _| state.borrow_mut().regions.clear(),
            move |_, _, window, cx| this.install(window, cx),
        )
        .absolute()
        .size_0()
    }

    fn install(&self, window: &mut Window, cx: &mut App) {
        if cx.reduce_motion() || self.0.borrow().tau.is_none() {
            return;
        }
        let movement = self.0.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, _| {
            if phase == DispatchPhase::Capture {
                movement.borrow_mut().retain_owner(event.position);
            }
        });
        let state = self.0.clone();
        let line_height = window.line_height();
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            if state.borrow().injecting {
                return;
            }
            // Native precise scrolling takes over without adding a wheel tail.
            if event.delta.precise() || event.source != ScrollSource::Pointer {
                state.borrow_mut().reset_motion();
                return;
            }
            let delta = event.delta.pixel_delta(line_height);
            let (mut dx, mut dy): (f32, f32) = (delta.x.into(), delta.y.into());
            let now = cx.background_executor().now();
            {
                let mut s = state.borrow_mut();
                let owner = s.region_at(event.position);
                if s.owner != owner {
                    s.reset_motion();
                }
                let Some(owner) = owner else {
                    // Unregistered native controls keep their normal wheel
                    // behavior rather than borrowing a window-wide gesture.
                    return;
                };
                if s.gesture.is_none() {
                    s.gesture = Some(tracing::info_span!(
                        "scroll.gesture",
                        scroll.owner = %owner,
                        scroll.curve = ?s.curve,
                        scroll.tau_ms = s.tau.map_or(0, |t| t.as_millis() as u64),
                        scroll.inertia = s.inertia.is_some(),
                    ));
                }
                if let Some(span) = &s.gesture {
                    tracing::debug!(
                        parent: span,
                        scroll.owner = %owner,
                        scroll.regions = %s.regions.iter().map(|(id, _)| id.as_ref()).collect::<Vec<_>>().join(","),
                        pointer_y = f32::from(event.position.y),
                        wheel_dy = dy,
                        remaining_y = s.remaining.y,
                        "wheel notch"
                    );
                }
                s.owner = Some(owner);
                // A new notch takes over from any coast.
                s.stretch = 1.;
                if let Some(inertia) = s.inertia {
                    let gap = s.spin.last_notch.map(|t| now.saturating_duration_since(t));
                    let same_dir = |v: f32, d: f32| d == 0. || v == 0. || (v >= 0.) == (d >= 0.);
                    let continuous = gap.is_some_and(|g| g < SPIN_GAP)
                        && same_dir(s.spin.speed.x, dx)
                        && same_dir(s.spin.speed.y, dy);
                    s.spin.streak = if continuous { s.spin.streak + 1 } else { 0 };
                    let factor = (1. + s.spin.streak as f32 * inertia.accel).min(inertia.accel_max);
                    dx *= factor;
                    dy *= factor;
                    s.spin.speed = match gap.filter(|_| continuous) {
                        Some(g) => {
                            let secs = g.as_secs_f32().max(0.016);
                            let blend = |v: f32, d: f32| 0.6 * v + 0.4 * (d / secs);
                            point(blend(s.spin.speed.x, dx), blend(s.spin.speed.y, dy))
                        }
                        // First notch of a spin: no speed known yet.
                        None => Point::default(),
                    };
                    s.spin.last_notch = Some(now);
                }
                // Same direction: accumulate so a spinning wheel keeps speed.
                // Reversal: drop what's left (and any velocity) and go the new way.
                let reversed = |rem: f32, d: f32| d != 0. && (rem >= 0.) != (d >= 0.);
                let merge = |rem: f32, d: f32| if reversed(rem, d) { d } else { rem + d };
                if reversed(s.remaining.x, dx) {
                    s.velocity.x = 0.;
                }
                if reversed(s.remaining.y, dy) {
                    s.velocity.y = 0.;
                }
                s.remaining = point(merge(s.remaining.x, dx), merge(s.remaining.y, dy));
            }
            cx.stop_propagation();
            Self::ensure_animating(&state, window, now);
        });
    }

    fn ensure_animating(state: &Rc<RefCell<State>>, window: &Window, now: Instant) {
        {
            let mut s = state.borrow_mut();
            if s.animating {
                return;
            }
            s.animating = true;
            s.last_tick = Some(now);
        }
        Self::schedule(state.clone(), window);
    }

    /// Once the wheel has paused for `SPIN_GAP` after a fast spin, extend the
    /// motion by the coast distance and stretch its duration to match, so the
    /// current speed carries on and then decays. Fires once per spin.
    fn maybe_coast(s: &mut State, now: Instant) {
        let (Some(inertia), Some(last)) = (s.inertia, s.spin.last_notch) else {
            return;
        };
        if now.saturating_duration_since(last) < SPIN_GAP {
            return;
        }
        let (speed, streak) = (s.spin.speed, s.spin.streak);
        s.spin = Spin::default();
        if streak < 2 || speed.x.abs().max(speed.y.abs()) < COAST_MIN_SPEED {
            return;
        }
        // Already (nearly) stopped: starting a coast now would be a visible kick.
        let before = s.remaining.x.abs().max(s.remaining.y.abs());
        if s.tau.is_none() || before < 8. {
            return;
        }
        let reach = inertia.carry * inertia.friction.as_secs_f32();
        s.remaining = point(
            s.remaining.x + speed.x * reach,
            s.remaining.y + speed.y * reach,
        );
        // Ease-out speed is `remaining / (tau * stretch)`: growing both by the
        // same ratio keeps the current speed and only lengthens the tail.
        let after = s.remaining.x.abs().max(s.remaining.y.abs());
        s.stretch = (after / before).clamp(1., MAX_STRETCH);
    }

    fn schedule(state: Rc<RefCell<State>>, window: &Window) {
        window.on_next_frame(move |window, cx| Self::tick(state, window, cx));
    }

    fn tick(state: Rc<RefCell<State>>, window: &mut Window, cx: &mut App) {
        let now = cx.background_executor().now();
        let step = {
            let mut s = state.borrow_mut();
            s.retain_owner(window.mouse_position());
            if cx.reduce_motion() {
                s.reset_motion();
            }
            let dt = s
                .last_tick
                .map_or(MAX_DT, |t| now.saturating_duration_since(t))
                .min(MAX_DT);
            s.last_tick = Some(now);
            let dt_s = dt.as_secs_f32();
            Self::maybe_coast(&mut s, now);
            let tau = s.tau.map(|t| t.mul_f32(s.stretch));
            match (tau, s.curve) {
                // Switched off mid-animation: finish in one step.
                (None, _) => {
                    let step = s.remaining;
                    s.remaining = Point::default();
                    s.velocity = Point::default();
                    step
                }
                (Some(tau), Curve::EaseOut) => {
                    let f = 1. - (-(dt_s / tau.as_secs_f32())).exp();
                    // Move a fraction of what's left; at least a whole pixel so
                    // the tail doesn't crawl, and never past the target.
                    let step = point(ease_step(s.remaining.x, f), ease_step(s.remaining.y, f));
                    s.remaining = point(s.remaining.x - step.x, s.remaining.y - step.y);
                    step
                }
                (Some(tau), Curve::EaseInOut) => {
                    // Critically damped spring, advanced by its closed form so
                    // any frame length is stable. `omega` is chosen so the
                    // motion settles in about the same time as the ease-out of
                    // the same preset (95% at ~4.7/omega vs 3*tau).
                    let omega = 1.58 / tau.as_secs_f32();
                    let e = (-omega * dt_s).exp();
                    let advance = |rem: f32, v: f32| -> (f32, f32) {
                        // Displacement from the target is `-rem`.
                        let d = -rem;
                        let c = v + omega * d;
                        let d2 = (d + c * dt_s) * e;
                        let v2 = (v - c * omega * dt_s) * e;
                        (-d2, v2)
                    };
                    let (rx, vx) = advance(s.remaining.x, s.velocity.x);
                    let (ry, vy) = advance(s.remaining.y, s.velocity.y);
                    let step = point(s.remaining.x - rx, s.remaining.y - ry);
                    s.remaining = point(rx, ry);
                    s.velocity = point(vx, vy);
                    step
                }
            }
        };
        if step.x != 0. || step.y != 0. {
            {
                let mut s = state.borrow_mut();
                s.pending_input += step.y;
                crate::telemetry::record_step(f64::from(step.y));
                if let Some(span) = &s.gesture {
                    tracing::debug!(
                        parent: span,
                        step_y = step.y,
                        remaining_y = s.remaining.y,
                        scroll.owner = s.owner.as_deref().unwrap_or(""),
                        "smooth step injected"
                    );
                }
            }
            state.borrow_mut().injecting = true;
            window.dispatch_event(
                PlatformInput::Scroll(ScrollEvent {
                    position: window.mouse_position(),
                    delta: ScrollDelta::Pixels(point(px(step.x), px(step.y))),
                    modifiers: window.modifiers(),
                    touch_phase: TouchPhase::Moved,
                    source: ScrollSource::Pointer,
                }),
                cx,
            );
            state.borrow_mut().injecting = false;
        }
        let done = {
            let s = state.borrow();
            let settled = |rem: f32, v: f32| rem.abs() < 0.5 && v.abs() < 10.;
            // Keep ticking while a fast spin may still turn into a coast.
            let coast_pending =
                s.inertia.is_some() && s.spin.last_notch.is_some() && s.spin.streak >= 2;
            settled(s.remaining.x, s.velocity.x)
                && settled(s.remaining.y, s.velocity.y)
                && !coast_pending
        };
        if done {
            let mut s = state.borrow_mut();
            s.remaining = Point::default();
            s.velocity = Point::default();
            s.stretch = 1.;
            s.spin = Spin::default();
            s.animating = false;
            s.last_tick = None;
            s.owner = None;
            // Ends the gesture span.
            s.gesture = None;
        } else {
            Self::schedule(state, window);
        }
    }
}

/// One ease-out step: a fraction `f` of the remaining distance, at least a
/// whole pixel so the tail doesn't crawl, and never past the target. Below a
/// pixel left (or on non-finite input) it finishes in one step; this must
/// never panic, since `clamp(1., rem)` would for `rem` in `[0.5, 1)`.
fn ease_step(rem: f32, f: f32) -> f32 {
    let left = rem.abs();
    if !left.is_finite() || !f.is_finite() || left <= 1. {
        return if left.is_finite() { rem } else { 0. };
    }
    (rem * f).round().abs().max(1.).min(left).copysign(rem)
}

#[cfg(test)]
mod tests {
    use super::{Curve, SmoothScroll, State, ease_step};

    #[test]
    fn regions_use_nested_and_overlay_order_and_reset_all_motion() {
        use gpui_kit::{Bounds, point, px, size};
        let outer = Bounds::new(point(px(0.), px(0.)), size(px(500.), px(500.)));
        let inner = Bounds::new(point(px(20.), px(20.)), size(px(100.), px(100.)));
        let mut state = State {
            regions: vec![("outer".into(), outer), ("inner".into(), inner)],
            owner: Some("inner".into()),
            remaining: point(10., -600.),
            velocity: point(3., -100.),
            spin: super::Spin {
                streak: 4,
                ..Default::default()
            },
            stretch: 3.,
            ..Default::default()
        };
        state.retain_owner(point(px(40.), px(40.)));
        assert_eq!(state.remaining.y, -600.);
        // Crossing into the parent clears every part of the gesture.
        state.retain_owner(point(px(150.), px(40.)));
        assert_eq!(state.remaining, Default::default());
        assert_eq!(state.velocity, Default::default());
        assert_eq!(state.spin.streak, 0);
        assert_eq!(state.stretch, 1.);
        assert!(state.owner.is_none());
        // A later overlay wins even when larger than the nested block.
        state.regions.push(("overlay".into(), outer));
        assert_eq!(
            state.region_at(point(px(40.), px(40.))).as_deref(),
            Some("overlay")
        );
    }

    #[gpui_kit::test]
    fn wheel_motion_does_not_transfer_between_scroll_regions(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::test::{TestEventExt as _, TestSupportExt as _, TestWindowExt as _};
        use gpui_kit::{
            AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _,
            Render, ScrollDelta, ScrollHandle, StatefulInteractiveElement as _, Styled as _,
            Window, div, point, px,
        };
        struct Fixture {
            smooth: SmoothScroll,
            a: ScrollHandle,
            b: ScrollHandle,
        }
        impl Render for Fixture {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let area = |id, handle: &ScrollHandle| {
                    self.smooth.region(
                        id,
                        div()
                            .id(id)
                            .test_support()
                            .w(px(200.))
                            .h(px(120.))
                            .overflow_y_scroll()
                            .track_scroll(handle)
                            .child(div().h(px(1000.)).w_full()),
                    )
                };
                div().size_full().child(self.smooth.element()).child(
                    div()
                        .flex()
                        .gap_4()
                        .child(area("a", &self.a))
                        .child(area("b", &self.b)),
                )
            }
        }
        cx.update(gpui_kit::init);
        let smooth = SmoothScroll::default();
        smooth.set_preset("smooth", "ease_in_out", "strong");
        let a = ScrollHandle::new();
        let b = ScrollHandle::new();
        let (handle, _view) = cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |_, cx| {
                cx.new(|_| Fixture {
                    smooth: smooth.clone(),
                    a: a.clone(),
                    b: b.clone(),
                })
            })
            .unwrap()
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            a.set_offset(point(px(0.), -a.max_offset().y));
            window.render_frame(cx);
            window.hover("a", cx);
            for _ in 0..4 {
                window.scroll("a", ScrollDelta::Lines(point(0., -6.)), cx);
            }
            assert!(smooth.0.borrow().remaining.y < 0.);
            assert!(smooth.0.borrow().spin.streak > 0);
            assert_eq!(a.offset().y, -a.max_offset().y);
            let inside = window.find("a").bounds().center() + point(px(10.), px(0.));
            window.pointer_move(inside, None, cx);
            assert!(
                smooth.0.borrow().remaining.y < 0.,
                "moving within a region must preserve the gesture"
            );
            window.hover("b", cx);
            assert_eq!(smooth.0.borrow().remaining, Default::default());
            assert_eq!(smooth.0.borrow().velocity, Default::default());
            assert_eq!(smooth.0.borrow().spin.streak, 0);
            window.render_frame(cx);
            assert_eq!(
                b.offset(),
                Default::default(),
                "the new region must not inherit motion"
            );
            window.scroll("b", ScrollDelta::Lines(point(0., -1.)), cx);
            assert_eq!(
                smooth.0.borrow().spin.streak,
                0,
                "new region starts a fresh spin"
            );
            assert_eq!(smooth.0.borrow().owner.as_deref(), Some("b"));

            // Precise-device input remains native and cancels any wheel tail.
            window.scroll("b", ScrollDelta::Pixels(point(px(0.), px(-5.))), cx);
            assert_eq!(smooth.0.borrow().remaining, Default::default());
            assert!(smooth.0.borrow().owner.is_none());

            // Smoothing without inertia must have the same ownership rules.
            smooth.set_preset("normal", "ease_out", "off");
            window.hover("a", cx);
            window.scroll("a", ScrollDelta::Lines(point(0., -6.)), cx);
            assert!(smooth.0.borrow().remaining.y < 0.);
            let before = b.offset();
            window.hover("b", cx);
            window.render_frame(cx);
            assert_eq!(smooth.0.borrow().remaining, Default::default());
            assert_eq!(b.offset(), before);
        })
        .unwrap();
    }

    #[test]
    fn ease_step_never_panics_or_overshoots() {
        for rem in [
            0.0, 0.3, 0.5, 0.69999695, 0.99, 1.0, 1.4, 7.0, -0.7, -0.5, -3.2, 500.,
        ] {
            for f in [0.0, 0.01, 0.2, 0.9, 1.0] {
                let step = ease_step(rem, f);
                assert!(
                    step.abs() <= rem.abs() + f32::EPSILON,
                    "rem {rem} f {f} step {step}"
                );
                assert!(step == 0. || step.signum() == rem.signum());
            }
        }
        assert_eq!(ease_step(f32::NAN, 0.5), 0.);
        assert_eq!(ease_step(0.7, f32::NAN), 0.7);
    }

    /// Simulates `tick` for one axis and returns the per-frame steps.
    fn steps(curve: Curve, tau_ms: f32, distance: f32, frames: usize) -> Vec<f32> {
        let (dt, tau) = (1. / 60., tau_ms / 1000.);
        let (mut rem, mut v, mut out) = (distance, 0f32, Vec::new());
        for _ in 0..frames {
            let step = match curve {
                Curve::EaseOut => {
                    let f = 1. - (-(dt / tau)).exp();
                    super::ease_step(rem, f)
                }
                Curve::EaseInOut => {
                    let omega = 1.58 / tau;
                    let e = (-omega * dt).exp();
                    let d = -rem;
                    let c = v + omega * d;
                    let d2 = (d + c * dt) * e;
                    v = (v - c * omega * dt) * e;
                    rem + d2
                }
            };
            rem -= step;
            out.push(step);
            if rem.abs() < 0.5 && v.abs() < 10. {
                break;
            }
        }
        out
    }

    #[test]
    fn ease_in_out_ramps_up_then_down_and_settles_like_ease_out() {
        let out = steps(Curve::EaseOut, 90., 60., 120);
        let inout = steps(Curve::EaseInOut, 90., 60., 120);
        // Ease-out: biggest step first. Ease in-out: speed builds, peaks later.
        assert!(out[0] >= out[1]);
        let peak = inout
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap();
        assert!(peak >= 2, "peak at frame {peak}: {inout:?}");
        assert!(inout[0] < inout[peak]);
        // Both cover the distance without overshooting, in a similar time.
        let total: f32 = inout.iter().sum();
        assert!((total - 60.).abs() < 1., "{total}");
        assert!(inout.iter().all(|s| *s >= -0.01), "no overshoot: {inout:?}");
        let (a, b) = (out.len() as f32, inout.len() as f32);
        assert!(
            (a / b) > 0.5 && (a / b) < 2.,
            "ease-out {a} frames vs in-out {b}"
        );
    }
}

#[cfg(test)]
mod inertia_tests {
    use super::{INERTIAS, SPIN_GAP, SmoothScroll, Spin, State};
    use gpui_kit::point;
    use std::time::{Duration, Instant};

    #[test]
    fn fast_spin_coasts_with_continuous_speed() {
        let ss = SmoothScroll::default();
        ss.set_preset("normal", "ease_out", "light");
        let mut s: State = std::mem::take(&mut *ss.0.borrow_mut());
        let t0 = Instant::now();
        // Mid-spin: 60px left, wheel was turning at 2000 px/s, 3 notches in a row.
        s.remaining = point(0., 60.);
        s.spin = Spin {
            last_notch: Some(t0),
            streak: 3,
            speed: point(0., 2000.),
        };
        let tau = s.tau.unwrap().as_secs_f32();
        let speed_before = 60. / tau;

        // Still spinning: no coast yet.
        SmoothScroll::maybe_coast(&mut s, t0 + Duration::from_millis(50));
        assert_eq!(s.remaining.y, 60.);

        // Wheel stopped: distance extends and the motion is stretched.
        SmoothScroll::maybe_coast(&mut s, t0 + SPIN_GAP);
        let inertia = INERTIAS[1].2.unwrap();
        let reach = 2000. * inertia.carry * inertia.friction.as_secs_f32();
        assert!(
            (s.remaining.y - (60. + reach)).abs() < 0.01,
            "{}",
            s.remaining.y
        );
        assert!(s.stretch > 1.);
        // No kick: the coast continues at the speed the view already had.
        let speed_after = s.remaining.y / (tau * s.stretch);
        assert!(
            (speed_after - speed_before).abs() < speed_before * 0.01,
            "{speed_after} vs {speed_before}"
        );
        // Fires once per spin.
        let rem = s.remaining.y;
        SmoothScroll::maybe_coast(&mut s, t0 + SPIN_GAP * 2);
        assert_eq!(s.remaining.y, rem);
    }

    #[test]
    fn slow_or_single_notches_do_not_coast() {
        let ss = SmoothScroll::default();
        ss.set_preset("normal", "ease_out", "strong");
        let mut s: State = std::mem::take(&mut *ss.0.borrow_mut());
        let t0 = Instant::now();
        s.remaining = point(0., 60.);
        s.spin = Spin {
            last_notch: Some(t0),
            streak: 3,
            speed: point(0., 300.),
        };
        SmoothScroll::maybe_coast(&mut s, t0 + SPIN_GAP);
        // Too slow to coast: target and duration are untouched.
        assert_eq!((s.remaining.y, s.stretch), (60., 1.));
    }
}
