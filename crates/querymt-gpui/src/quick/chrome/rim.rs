//! macOS: the light along the panel's edge as Core Animation layers
//! (design D14), run by the system compositor.
//!
//! A layer-hosting view sits between the material and GPUI's view. Its layers:
//! a line along the whole edge for each colour of the light (accent, warn,
//! danger), and the highlight, in three pieces (the head and two fainter
//! tail segments) on a path that traces the outline twice, so that a tail can
//! cross the start of the outline. The glow is each layer's shadow; a mask of
//! the panel's own shape clips it to fall inward only.
//!
//! [`CoreAnimationRim::show`] turns the [`EdgeState`] into a
//! [`Plan`](crate::quick::edge_plan::Plan) (pure, and tested against
//! `frame_at`) and sets it as animations that start at that moment. After
//! that there is no frame callback and no work of ours on the main thread
//! until the next change of state, so a busy main thread does not stall the
//! light.

use std::cell::RefCell;
use std::rc::{Rc, Weak as RcWeak};
use std::time::Instant;

use gpui_kit::Window;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSWindowOrderingMode};
use objc2_core_graphics::{CGColor, CGMutablePath, CGPath};
use objc2_foundation::{MainThreadMarker, NSArray, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{
    CABasicAnimation, CACurrentMediaTime, CAKeyframeAnimation, CALayer, CAMediaTiming,
    CAMediaTimingFunction, CAShapeLayer, CATransaction, kCALineCapRound, kCALineJoinRound,
    kCAMediaTimingFunctionLinear,
};

use super::RADIUS;
use crate::quick::edge::{EdgeColours, EdgeError, EdgeLight, EdgeState, MOTION, Outline, Role};
use crate::quick::edge_plan::{PIECE_STRENGTH, PIECES, Plan, piece_range};

/// Points between the vertices of the outline's path.
const PATH_STEP: f32 = 2.;
const ROLES: [Role; 3] = [Role::Accent, Role::Warn, Role::Danger];

thread_local! {
    /// The layers of the panel that is open: one at a time, and AppKit is
    /// only touched on the main thread. Weak: the panel owns them.
    static LAYERS: RefCell<Option<RcWeak<Layers>>> = const { RefCell::new(None) };
}

struct Layers {
    host: Retained<NSView>,
    root: Retained<CALayer>,
    mask: Retained<CAShapeLayer>,
    /// One line along the edge for each of [`ROLES`].
    lines: [Retained<CAShapeLayer>; 3],
    /// The highlight, tail first.
    pieces: [Retained<CAShapeLayer>; PIECES],
}

fn ns(name: &str) -> Retained<NSString> {
    NSString::from_str(name)
}

fn any_number(x: f32) -> Retained<AnyObject> {
    // SAFETY: an `NSNumber` is an Objective-C object.
    unsafe { Retained::cast_unchecked(NSNumber::new_f32(x)) }
}

fn colour(c: [f32; 4]) -> objc2_core_foundation::CFRetained<CGColor> {
    CGColor::new_srgb(c[0] as f64, c[1] as f64, c[2] as f64, c[3] as f64)
}

fn shape(round: bool) -> Retained<CAShapeLayer> {
    let layer = CAShapeLayer::layer();
    layer.setFillColor(None);
    layer.setOpacity(0.);
    layer.setShadowOffset(NSSize::new(0., 0.));
    // `shadowRadius` is a blur radius; the light reaches about twice it.
    layer.setShadowRadius(MOTION.glow_width as f64 / 2.);
    layer.setShadowOpacity(0.);
    layer.setLineWidth(MOTION.line_width as f64);
    // SAFETY: AppKit's constants.
    unsafe {
        layer.setLineJoin(kCALineJoinRound);
        if round {
            layer.setLineCap(kCALineCapRound);
        }
    }
    layer
}

/// `outline` as a path in a layer `height` tall whose origin is at the bottom
/// left, so y is flipped. Traced `times` times in a row, closed if once.
fn path(
    outline: &Outline,
    height: f32,
    times: usize,
) -> objc2_core_foundation::CFRetained<CGMutablePath> {
    let points = outline.points(0., 1., PATH_STEP);
    let path = CGMutablePath::new();
    let mut first = true;
    for _ in 0..times {
        for (x, y) in &points {
            let (x, y) = (*x as f64, (height - *y) as f64);
            // SAFETY: a null transform is allowed.
            unsafe {
                if std::mem::take(&mut first) {
                    CGMutablePath::move_to_point(Some(&path), std::ptr::null(), x, y);
                } else {
                    CGMutablePath::add_line_to_point(Some(&path), std::ptr::null(), x, y);
                }
            }
        }
    }
    if times == 1 {
        CGMutablePath::close_subpath(Some(&path));
    }
    path
}

fn keyframes(
    layer: &CALayer,
    key: &str,
    name: &str,
    begin: f64,
    duration: f64,
    repeat: bool,
    keys: &[(f32, f32)],
) {
    let anim = CAKeyframeAnimation::animationWithKeyPath(Some(&ns(key)));
    let values: Vec<_> = keys.iter().map(|(_, v)| any_number(*v)).collect();
    let times: Vec<_> = keys.iter().map(|(t, _)| NSNumber::new_f32(*t)).collect();
    // SAFETY: the values are `NSNumber`s, which these properties animate.
    unsafe { anim.setValues(Some(&NSArray::from_retained_slice(&values))) };
    anim.setKeyTimes(Some(&NSArray::from_retained_slice(&times)));
    anim.setDuration(duration);
    anim.setBeginTime(begin);
    anim.setRepeatCount(if repeat { f32::INFINITY } else { 1. });
    layer.addAnimation_forKey(&anim, Some(&ns(name)));
}

fn ramp(
    layer: &CALayer,
    key: &str,
    name: &str,
    begin: f64,
    duration: f64,
    repeat: bool,
    ft: (f32, f32),
) {
    let anim = CABasicAnimation::animationWithKeyPath(Some(&ns(key)));
    // SAFETY: `NSNumber`s, which these properties animate; a constant.
    unsafe {
        anim.setFromValue(Some(&any_number(ft.0)));
        anim.setToValue(Some(&any_number(ft.1)));
        anim.setTimingFunction(Some(&CAMediaTimingFunction::functionWithName(
            kCAMediaTimingFunctionLinear,
        )));
    }
    anim.setDuration(duration);
    anim.setBeginTime(begin);
    anim.setRepeatCount(if repeat { f32::INFINITY } else { 1. });
    layer.addAnimation_forKey(&anim, Some(&ns(name)));
}

impl Layers {
    /// Fit the layers to the host's current bounds: paths rebuilt with
    /// Core Animation's implicit animations off, or it would animate the
    /// change by itself.
    fn relayout(&self) {
        let bounds = self.host.bounds();
        let (w, h) = (bounds.size.width as f32, bounds.size.height as f32);
        if w <= 0. || h <= 0. {
            return;
        }
        let frame = NSRect::new(NSPoint::new(0., 0.), bounds.size);
        let outline = Outline::new(w, h, RADIUS).inset(MOTION.line_width / 2.);
        let (single, double) = (path(&outline, h, 1), path(&outline, h, 2));
        let (single, double): (&CGPath, &CGPath) = (&single, &double);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        // SAFETY: a null transform is allowed.
        let rounded = unsafe {
            let r = RADIUS as f64;
            CGPath::with_rounded_rect(frame, r, r, std::ptr::null())
        };
        self.mask.setFrame(frame);
        self.mask.setPath(Some(&rounded));
        for line in &self.lines {
            line.setFrame(frame);
            line.setPath(Some(single));
        }
        for piece in &self.pieces {
            piece.setFrame(frame);
            piece.setPath(Some(double));
        }
        CATransaction::commit();
    }

    /// Set `plan` as the layers' animations, starting at media time `now`.
    fn apply(&self, plan: &Plan, colours: &EdgeColours, now: f64) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let layers = self.lines.iter().chain(&self.pieces);
        for layer in layers {
            layer.removeAllAnimations();
        }
        // Colours and the resting values, which show once the animations end.
        let hold = plan.hold;
        for (i, line) in self.lines.iter().enumerate() {
            let c = colour(colours.of(ROLES[i]));
            line.setStrokeColor(Some(&c));
            line.setShadowColor(Some(&c));
            line.setOpacity(hold.line[i]);
            line.setShadowOpacity(hold.line_glow[i]);
        }
        let accent = colour(colours.accent);
        for (j, piece) in self.pieces.iter().enumerate() {
            piece.setStrokeColor(Some(&accent));
            piece.setShadowColor(Some(&accent));
            piece.setOpacity(PIECE_STRENGTH[j] * hold.highlight);
            piece.setLineWidth(hold.width as f64);
            piece.setShadowOpacity(hold.glow);
            let (start, end) = piece_range(plan.hold_head, plan.tail, j);
            piece.setStrokeStart(start as f64);
            piece.setStrokeEnd(end as f64);
        }
        self.root.setHidden(plan.idle());

        for (n, track) in plan.tracks.iter().enumerate() {
            let begin = now + track.begin;
            let each = |pick: &dyn Fn(&crate::quick::edge_plan::Levels) -> f32| {
                track
                    .keys
                    .iter()
                    .map(|(t, levels)| (*t, pick(levels)))
                    .collect::<Vec<_>>()
            };
            let run = |layer: &CALayer, key: &str, keys: Vec<(f32, f32)>| {
                keyframes(
                    layer,
                    key,
                    &format!("{key}-{n}"),
                    begin,
                    track.duration,
                    track.repeat,
                    &keys,
                );
            };
            for (i, line) in self.lines.iter().enumerate() {
                run(line, "opacity", each(&|l| l.line[i]));
                run(line, "shadowOpacity", each(&|l| l.line_glow[i]));
            }
            for (j, piece) in self.pieces.iter().enumerate() {
                run(piece, "opacity", each(&|l| PIECE_STRENGTH[j] * l.highlight));
                run(piece, "lineWidth", each(&|l| l.width));
                run(piece, "shadowOpacity", each(&|l| l.glow));
            }
        }
        for (n, leg) in plan.legs.iter().enumerate() {
            for (j, piece) in self.pieces.iter().enumerate() {
                let (from, to) = (
                    piece_range(leg.from, plan.tail, j),
                    piece_range(leg.to, plan.tail, j),
                );
                let begin = now + leg.begin;
                ramp(
                    piece,
                    "strokeStart",
                    &format!("start-{n}"),
                    begin,
                    leg.duration,
                    leg.repeat,
                    (from.0, to.0),
                );
                ramp(
                    piece,
                    "strokeEnd",
                    &format!("end-{n}"),
                    begin,
                    leg.duration,
                    leg.repeat,
                    (from.1, to.1),
                );
            }
        }
        CATransaction::commit();
    }
}

/// The light as Core Animation layers.
pub(crate) struct CoreAnimationRim {
    layers: Rc<Layers>,
    /// Where the edge's clock starts, to tell a state's times from now.
    origin: Instant,
}

impl CoreAnimationRim {
    /// Add the light's view between the material and GPUI's view. `origin` is
    /// the edge's clock. Fails when the view hierarchy is not what is
    /// expected; the panel then draws the ring.
    pub(crate) fn install(window: &Window, origin: Instant) -> Result<Self, EdgeError> {
        let fail = |what: &str| EdgeError(format!("edge light: {what}"));
        let mtm = MainThreadMarker::new().ok_or_else(|| fail("not on the main thread"))?;
        let gpui = super::macos::gpui_view(window).ok_or_else(|| fail("no drawing view"))?;
        // SAFETY: a plain getter on a live view, on the main thread.
        let parent = unsafe { gpui.superview() }.ok_or_else(|| fail("no content view"))?;
        let host = NSView::initWithFrame(mtm.alloc(), parent.bounds());
        let root = CALayer::layer();
        // A layer-hosting view: the layer is set before the view wants one.
        host.setLayer(Some(&root));
        host.setWantsLayer(true);
        host.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let mask = CAShapeLayer::layer();
        let black = colour([0., 0., 0., 1.]);
        mask.setFillColor(Some(&black));
        // SAFETY: the mask is a layer that is not otherwise used.
        unsafe { root.setMask(Some(&mask)) };
        let lines = std::array::from_fn(|_| shape(false));
        let pieces = std::array::from_fn(|_| shape(true));
        for layer in lines.iter().chain(&pieces) {
            root.addSublayer(layer);
        }
        root.setHidden(true);
        // Directly below GPUI's view: above the material, which went to the
        // bottom.
        parent.addSubview_positioned_relativeTo(&host, NSWindowOrderingMode::Below, Some(gpui));
        let layers = Rc::new(Layers {
            host,
            root,
            mask,
            lines,
            pieces,
        });
        layers.relayout();
        LAYERS.with(|slot| *slot.borrow_mut() = Some(Rc::downgrade(&layers)));
        Ok(Self { layers, origin })
    }
}

impl EdgeLight for CoreAnimationRim {
    fn show(&mut self, state: &EdgeState, colours: &EdgeColours) -> Result<(), EdgeError> {
        let t = Instant::now()
            .saturating_duration_since(self.origin)
            .as_secs_f64();
        let plan = Plan::new(state, t);
        self.layers.apply(&plan, colours, CACurrentMediaTime());
        Ok(())
    }

    fn resize(&mut self, _width: f32, _height: f32) -> Result<(), EdgeError> {
        // The host's own bounds are what the paths follow: during the
        // opening they are smaller than the window.
        self.layers.relayout();
        Ok(())
    }
}

fn current() -> Option<Rc<Layers>> {
    LAYERS.with(|slot| slot.borrow().as_ref().and_then(RcWeak::upgrade))
}

/// One step of the opening: the light's view scales with the material, and
/// its paths follow.
pub(super) fn open_step(scale: f64) {
    let Some(layers) = current() else {
        return;
    };
    // SAFETY: a plain getter on a live view, on the main thread.
    if let Some(parent) = unsafe { layers.host.superview() } {
        layers
            .host
            .setFrame(super::macos::scaled(parent.bounds(), scale));
    }
    layers.relayout();
}

/// The window has a new size: the paths follow.
pub(super) fn relayout() {
    if let Some(layers) = current() {
        layers.relayout();
    }
}
