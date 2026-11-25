//! The light along the panel's edge (design D14): one model of its motion,
//! and the trait its renderers implement.
//!
//! Pure logic, built on every platform so its tests run everywhere:
//!
//! - [`MOTION`] holds every constant of the light.
//! - [`Edge`] turns the [`Activity`] of the shown session, its stream rate
//!   and the clock into an [`EdgeState`]: what is shown, since when, where
//!   the highlight is anchored and how fast it travels.
//! - [`frame_at`] says what the light looks like at a time: this is the
//!   definition of the motion. The GPUI ring draws it frame by frame; a
//!   renderer that animates by itself (Core Animation on macOS) derives its
//!   animations from the same state, so the two cannot drift.
//! - [`EdgeLight`] is what a renderer outside GPUI implements. Its calls can
//!   fail; the panel then draws the ring instead.
//! - [`Outline`] is the rounded rectangle the light runs along, measured by
//!   arc length, so a position means the same to every renderer.

use std::time::Instant;

use super::activity::{Activity, Phase};

/// The light's constants (design D14). Times are in seconds, sizes in
/// points.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgeMotion {
    /// Width of the line on the edge.
    pub(crate) line_width: f32,
    /// How far the glow reaches inward, and how strong it is at most.
    pub(crate) glow_width: f32,
    pub(crate) glow_alpha: f32,
    /// A lap of the highlight for a slow stream, and for a fast one.
    pub(crate) lap_slow: f64,
    pub(crate) lap_fast: f64,
    /// The lap time changes in steps of this, at most once per interval.
    pub(crate) lap_step: f64,
    pub(crate) lap_interval: f64,
    /// Length of the highlight's tail, as a share of the perimeter.
    pub(crate) tail: f32,
    /// The line under the travelling highlight.
    pub(crate) base_line: f32,
    /// The line when motion is not allowed.
    pub(crate) still_line: f32,
    pub(crate) still_halo: f32,
    /// A flare on a tool call.
    pub(crate) flare: f64,
    /// The breath while the agent waits, and its range.
    pub(crate) breath: f64,
    pub(crate) breath_low: f32,
    pub(crate) breath_high: f32,
    /// The pulse of an error.
    pub(crate) pulse: f64,
    /// At most this long to finish the lap at the end of a run, then the
    /// edge lights once and fades over `finish`.
    pub(crate) finish_run: f64,
    pub(crate) finish: f64,
    /// Changes cross-fade over this.
    pub(crate) fade: f64,
    /// The light waits this long into a run before it appears.
    pub(crate) delay: f64,
    /// A run shorter than this ends with a fade, without the finishing lap.
    pub(crate) min_finish_run: f64,
}

pub(crate) const MOTION: EdgeMotion = EdgeMotion {
    line_width: 1.5,
    glow_width: 14.,
    glow_alpha: 0.12,
    lap_slow: 2.4,
    lap_fast: 1.2,
    lap_step: 0.1,
    lap_interval: 0.25,
    tail: 0.12,
    base_line: 0.22,
    still_line: 0.6,
    still_halo: 0.3,
    flare: 0.25,
    breath: 2.4,
    breath_low: 0.25,
    breath_high: 0.7,
    pulse: 0.6,
    finish_run: 0.8,
    finish: 0.7,
    fade: 0.2,
    delay: 0.15,
    min_finish_run: 1.,
};

/// What the light shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Show {
    #[default]
    Off,
    /// A highlight travels round the edge.
    Travel,
    /// The whole edge breathes: the agent waits for the user.
    Breathe,
    /// One pulse: a run ended with an error.
    Pulse,
    /// The highlight finishes its lap, then the edge lights once and fades.
    Finish,
}

/// Which colour of the palette a part of the light takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Role {
    #[default]
    Accent,
    Warn,
    Danger,
}

/// The colours of the light, premultiplied by nothing: plain RGBA from 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EdgeColours {
    pub(crate) accent: [f32; 4],
    pub(crate) warn: [f32; 4],
    pub(crate) danger: [f32; 4],
}

impl EdgeColours {
    pub(crate) fn of(&self, role: Role) -> [f32; 4] {
        match role {
            Role::Accent => self.accent,
            Role::Warn => self.warn,
            Role::Danger => self.danger,
        }
    }
}

/// One thing the light shows, from `since` on. The highlight's position, in
/// laps, is `anchor + (t - anchor_t) / lap`; changing the lap time
/// re-anchors it, so it never jumps.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Part {
    pub(crate) show: Show,
    pub(crate) since: f64,
    pub(crate) anchor: f64,
    pub(crate) anchor_t: f64,
    /// Seconds per lap.
    pub(crate) lap: f64,
    /// When the highlight last flared.
    pub(crate) flare_at: Option<f64>,
}

impl Part {
    fn off(t: f64) -> Self {
        Self {
            since: t,
            anchor_t: t,
            lap: MOTION.lap_slow,
            ..Self::default()
        }
    }

    /// The highlight's position at `t`, in laps (not wrapped).
    pub(crate) fn position(&self, t: f64) -> f64 {
        self.anchor + (t - self.anchor_t) / self.lap
    }

    /// A finish: when its highlight reaches the end of the lap, and where
    /// that is in laps.
    pub(crate) fn finish_target(&self) -> (f64, f64) {
        let target = self.anchor.floor() + 1.;
        (self.anchor_t + (target - self.anchor) * self.lap, target)
    }

    /// When this part ends by itself, if it does.
    pub(crate) fn ends_at(&self) -> Option<f64> {
        match self.show {
            Show::Pulse => Some(self.since + MOTION.pulse),
            Show::Finish => Some(self.finish_target().0 + MOTION.finish),
            _ => None,
        }
    }
}

/// What the light shows: a part, and the one it is fading out from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct EdgeState {
    pub(crate) part: Part,
    /// The part shown before, and when it was replaced: it fades out over
    /// [`EdgeMotion::fade`] while `part` fades in.
    pub(crate) prev: Option<(Part, f64)>,
    /// Motion is allowed: false under reduced motion or Low Power Mode.
    pub(crate) moving: bool,
    /// When this state was made, on the edge's clock.
    pub(crate) at: f64,
}

impl EdgeState {
    /// Something is drawn at `t`.
    pub(crate) fn visible(&self, t: f64) -> bool {
        self.part.show != Show::Off || self.fading(t)
    }

    /// The previous part is still fading out at `t`.
    pub(crate) fn fading(&self, t: f64) -> bool {
        self.prev.is_some_and(|(_, at)| t < at + MOTION.fade)
    }

    /// The light changes with time at `t` (it is not one still frame).
    pub(crate) fn animating(&self, t: f64) -> bool {
        self.fading(t) || (self.part.show != Show::Off && self.moving)
    }
}

/// What the light looks like at a time. Every strength is from 0 to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct EdgeFrame {
    /// The highlight's head along the perimeter, from 0 to 1 (0 is the
    /// middle of the top edge, going clockwise).
    pub(crate) head: f32,
    /// The tail's length, as a share of the perimeter.
    pub(crate) tail: f32,
    /// Strength of the travelling highlight, in the accent.
    pub(crate) highlight: f32,
    /// A flare of the highlight: wider and brighter.
    pub(crate) flare: f32,
    /// Strength of the glow inward of the highlight.
    pub(crate) glow: f32,
    /// Strength of the line along the whole edge, and its colour.
    pub(crate) line: f32,
    pub(crate) line_role: Role,
    /// Strength of the glow inward of the whole edge.
    pub(crate) halo: f32,
}

fn smoothstep(x: f64) -> f32 {
    let x = x.clamp(0., 1.);
    (x * x * (3. - 2. * x)) as f32
}

/// The share of a lap `laps` is into it, from 0 up to but not 1: a value
/// just under a whole lap would round to 1 as an `f32`.
fn wrap(laps: f64) -> f32 {
    let share = laps.rem_euclid(1.) as f32;
    if share >= 1. { 0. } else { share }
}

fn flare_at(part: &Part, t: f64) -> f32 {
    match part.flare_at {
        Some(at) if t >= at && t < at + MOTION.flare => {
            (std::f64::consts::PI * (t - at) / MOTION.flare).sin() as f32
        }
        _ => 0.,
    }
}

/// One part at `t`, at full strength.
fn part_frame(part: &Part, t: f64, moving: bool) -> EdgeFrame {
    let still = EdgeFrame {
        line: MOTION.still_line,
        halo: MOTION.still_halo,
        ..EdgeFrame::default()
    };
    match part.show {
        Show::Off => EdgeFrame::default(),
        Show::Travel if !moving => still,
        Show::Travel => EdgeFrame {
            head: wrap(part.position(t)),
            tail: MOTION.tail,
            highlight: 1.,
            flare: flare_at(part, t),
            glow: 1.,
            line: MOTION.base_line,
            ..EdgeFrame::default()
        },
        Show::Breathe if !moving => EdgeFrame {
            line_role: Role::Warn,
            ..still
        },
        Show::Breathe => {
            let phase = (t - part.since) / MOTION.breath * std::f64::consts::TAU;
            let b = (0.5 - 0.5 * phase.cos()) as f32;
            let line = MOTION.breath_low + (MOTION.breath_high - MOTION.breath_low) * b;
            EdgeFrame {
                line,
                line_role: Role::Warn,
                halo: line * 0.6,
                ..EdgeFrame::default()
            }
        }
        Show::Pulse => {
            let u = (t - part.since) / MOTION.pulse;
            if !(0. ..1.).contains(&u) {
                return EdgeFrame::default();
            }
            let s = (std::f64::consts::PI * u).sin() as f32;
            EdgeFrame {
                line: s,
                line_role: Role::Danger,
                halo: s,
                ..EdgeFrame::default()
            }
        }
        Show::Finish => {
            let (run_end, target) = part.finish_target();
            if t < run_end {
                return EdgeFrame {
                    head: wrap(part.position(t).min(target)),
                    tail: MOTION.tail,
                    highlight: 1.,
                    glow: 1.,
                    line: MOTION.base_line,
                    ..EdgeFrame::default()
                };
            }
            let v = (t - run_end) / MOTION.finish;
            if v >= 1. {
                return EdgeFrame::default();
            }
            // Up quickly, then a long fade.
            let lit = if v < 0.25 {
                smoothstep(v / 0.25)
            } else {
                1. - smoothstep((v - 0.25) / 0.75)
            };
            // The base line goes out with the rest, so the end is not a pop
            // from `base_line` to nothing.
            let base = MOTION.base_line * (1. - smoothstep((v - 0.5) / 0.5));
            EdgeFrame {
                head: wrap(target),
                tail: MOTION.tail,
                highlight: (1. - v * 4.).max(0.) as f32,
                glow: (1. - v * 4.).max(0.) as f32,
                line: base + (1. - MOTION.base_line) * lit,
                halo: lit,
                ..EdgeFrame::default()
            }
        }
    }
}

/// The light at `t` (design D14): the definition of its motion.
pub(crate) fn frame_at(state: &EdgeState, t: f64) -> EdgeFrame {
    let now = part_frame(&state.part, t, state.moving);
    let Some((prev, at)) = state.prev.filter(|_| state.fading(t)) else {
        return now;
    };
    // Linear, so a highlight that continues (a travel into its finish)
    // keeps its strength through the change.
    let f = smoothstep((t - at) / MOTION.fade);
    let before = part_frame(&prev, t, state.moving);
    let mix = |a: f32, b: f32| a * (1. - f) + b * f;
    let highlight_from_now = now.highlight > 0.;
    let (head, tail) = if highlight_from_now || before.highlight == 0. {
        (now.head, now.tail)
    } else {
        (before.head, before.tail)
    };
    let line_role = if now.line * f >= before.line * (1. - f) {
        now.line_role
    } else {
        before.line_role
    };
    EdgeFrame {
        head,
        tail,
        highlight: mix(before.highlight, now.highlight),
        flare: mix(before.flare, now.flare),
        glow: mix(before.glow, now.glow),
        line: mix(before.line, now.line),
        line_role,
        halo: mix(before.halo, now.halo),
    }
}

/// `now` on a clock starting at `origin`, in seconds.
pub(crate) fn secs_since(origin: Instant, now: Instant) -> f64 {
    now.saturating_duration_since(origin).as_secs_f64()
}

/// What an update of the [`Edge`] asks of the panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Update {
    /// The state changed: renderers that animate by themselves are told.
    pub(crate) changed: bool,
    /// A flare, a pulse or a finish is over, or a run ended with nothing to
    /// show: the activity tracker is settled.
    pub(crate) settle: bool,
}

/// Turns the activity of the shown session into what the light shows.
#[derive(Debug)]
pub(crate) struct Edge {
    origin: Instant,
    state: EdgeState,
    /// When the current run began, on the edge's clock.
    run_start: Option<f64>,
    /// When the lap time last changed.
    lap_changed: f64,
    /// Sparks already seen.
    sparks: u32,
}

impl Edge {
    pub(crate) fn new(origin: Instant) -> Self {
        Self {
            origin,
            state: EdgeState {
                part: Part::off(0.),
                moving: true,
                ..EdgeState::default()
            },
            run_start: None,
            lap_changed: f64::NEG_INFINITY,
            sparks: 0,
        }
    }

    /// `now` on the edge's clock, in seconds.
    pub(crate) fn secs(&self, now: Instant) -> f64 {
        secs_since(self.origin, now)
    }

    /// Where the edge's clock starts.
    pub(crate) fn origin(&self) -> Instant {
        self.origin
    }

    pub(crate) fn state(&self) -> &EdgeState {
        &self.state
    }

    /// The lap time for a stream `rate` (0 to 1), in steps.
    fn lap_for(rate: f32) -> f64 {
        let lap = MOTION.lap_slow + (MOTION.lap_fast - MOTION.lap_slow) * rate.clamp(0., 1.) as f64;
        (lap / MOTION.lap_step).round() * MOTION.lap_step
    }

    /// Show `show` from `t` on, fading from what was shown if it was visible
    /// and `fade` is asked for.
    fn switch(&mut self, t: f64, part: Part, fade: bool) {
        let shown = self.state.part.show != Show::Off;
        self.state.prev = (fade && shown).then_some((self.state.part, t));
        self.state.part = part;
    }

    fn travel(&self, t: f64, rate: f32) -> Part {
        Part {
            show: Show::Travel,
            since: t,
            anchor: 0.,
            anchor_t: t,
            lap: Self::lap_for(rate),
            flare_at: None,
        }
    }

    /// Take the activity at `now`. `rate` is the stream rate (0 to 1);
    /// `moving`: motion is allowed.
    pub(crate) fn update(
        &mut self,
        now: Instant,
        activity: Activity,
        rate: f32,
        moving: bool,
    ) -> Update {
        let t = self.secs(now);
        let before = self.state;
        let mut settle = false;

        // What ends by itself.
        if let Some(end) = self.state.part.ends_at()
            && t >= end
        {
            self.switch(t, Part::off(t), false);
            self.state.prev = None;
            settle = true;
        }
        self.state.moving = moving;

        let new_sparks = activity.sparks != self.sparks;
        self.sparks = activity.sparks;
        let show = self.state.part.show;
        match activity.phase {
            Phase::Idle => {
                // Another session was shown, or a settled end: off at once.
                self.run_start = None;
                if matches!(show, Show::Travel | Show::Breathe) {
                    self.switch(t, Part::off(t), false);
                }
            }
            Phase::Running => {
                let start = *self.run_start.get_or_insert(t);
                match show {
                    Show::Off if t - start < MOTION.delay => {}
                    Show::Travel => {
                        let lap = Self::lap_for(rate);
                        let part = &mut self.state.part;
                        if moving && lap != part.lap && t - self.lap_changed >= MOTION.lap_interval
                        {
                            part.anchor = part.position(t);
                            part.anchor_t = t;
                            part.lap = lap;
                            self.lap_changed = t;
                        }
                        if new_sparks && moving {
                            part.flare_at = Some(t);
                        }
                    }
                    _ => {
                        let part = self.travel(t, rate);
                        self.lap_changed = t;
                        self.switch(t, part, true);
                    }
                }
            }
            Phase::Waiting => {
                self.run_start.get_or_insert(t);
                if show != Show::Breathe {
                    let part = Part {
                        show: Show::Breathe,
                        ..Part::off(t)
                    };
                    self.switch(t, part, true);
                }
            }
            Phase::Error => {
                self.run_start = None;
                match show {
                    Show::Pulse => {}
                    Show::Off => settle = true,
                    _ if !moving => {
                        self.switch(t, Part::off(t), false);
                        settle = true;
                    }
                    _ => {
                        let part = Part {
                            show: Show::Pulse,
                            ..Part::off(t)
                        };
                        self.switch(t, part, true);
                    }
                }
            }
            Phase::Done => {
                let ran = self.run_start.take().map(|start| t - start);
                match show {
                    Show::Finish | Show::Pulse => {}
                    Show::Off => settle = true,
                    _ if !moving => {
                        self.switch(t, Part::off(t), false);
                        settle = true;
                    }
                    Show::Travel if ran.is_some_and(|ran| ran >= MOTION.min_finish_run) => {
                        let travel = self.state.part;
                        let at = travel.position(t);
                        let left = at.floor() + 1. - at;
                        // At the travel's speed, or faster when the lap's end
                        // is too far to reach in time.
                        let lap = travel.lap.min(MOTION.finish_run / left.max(1e-6));
                        let part = Part {
                            show: Show::Finish,
                            since: t,
                            anchor: at,
                            anchor_t: t,
                            lap,
                            flare_at: None,
                        };
                        self.switch(t, part, false);
                        self.state.prev = None;
                    }
                    _ => {
                        // A short run, or one that ended waiting: a fade.
                        self.switch(t, Part::off(t), true);
                        settle = true;
                    }
                }
            }
        }
        let changed = self.state.part != before.part
            || self.state.prev != before.prev
            || self.state.moving != before.moving;
        if changed {
            self.state.at = t;
        }
        Update { changed, settle }
    }

    /// When the edge next changes without new activity, on its clock: the
    /// light appearing after its delay, a pulse or a finish ending, a fade
    /// ending, and the lap time to check again while the highlight travels.
    pub(crate) fn next_wake(&self, now: Instant) -> Option<f64> {
        let t = self.secs(now);
        let part = &self.state.part;
        let mut wakes = Vec::new();
        if part.show == Show::Off
            && let Some(start) = self.run_start
        {
            wakes.push(start + MOTION.delay);
        }
        wakes.extend(part.ends_at());
        if let Some((_, at)) = self.state.prev {
            wakes.push(at + MOTION.fade);
        }
        if part.show == Show::Travel && self.state.moving {
            wakes.push((self.lap_changed + MOTION.lap_interval).max(t + MOTION.lap_interval));
        }
        wakes.into_iter().filter(|w| *w > t).reduce(f64::min)
    }
}

/// Why a renderer of the light gave up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeError(pub(crate) String);

impl std::fmt::Display for EdgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EdgeError {}

/// A renderer of the light that draws outside GPUI (design D14): Core
/// Animation on macOS, and Metal if it is ever wanted. Every call can fail;
/// the panel then drops the renderer and draws the ring itself.
pub(crate) trait EdgeLight {
    /// Show `state` from now on, in `colours`. A state that shows nothing
    /// hides the light.
    fn show(&mut self, state: &EdgeState, colours: &EdgeColours) -> Result<(), EdgeError>;
    /// The panel is now `width` by `height` points.
    fn resize(&mut self, width: f32, height: f32) -> Result<(), EdgeError>;
}

/// The rounded rectangle the light runs along, measured by arc length from
/// the middle of its top edge, clockwise, with y down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Outline {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
    pub(crate) r: f32,
}

/// One piece of an outline: a straight edge or a quarter circle.
enum Piece {
    Line { from: (f32, f32), to: (f32, f32) },
    Arc { centre: (f32, f32), start: f32 },
}

impl Outline {
    pub(crate) fn new(w: f32, h: f32, r: f32) -> Self {
        Self {
            x: 0.,
            y: 0.,
            w: w.max(0.),
            h: h.max(0.),
            r: r.clamp(0., w.min(h).max(0.) / 2.),
        }
    }

    /// The same outline, `by` points further in.
    pub(crate) fn inset(&self, by: f32) -> Self {
        let mut inner = Self::new(self.w - 2. * by, self.h - 2. * by, self.r - by);
        inner.x = self.x + by;
        inner.y = self.y + by;
        inner
    }

    fn pieces(&self) -> [(Piece, f32); 9] {
        let (x, y, w, h, r) = (self.x, self.y, self.w, self.h, self.r);
        let quarter = std::f32::consts::FRAC_PI_2 * r;
        let line = |from: (f32, f32), to: (f32, f32)| {
            let len = ((to.0 - from.0).powi(2) + (to.1 - from.1).powi(2)).sqrt();
            (Piece::Line { from, to }, len)
        };
        let arc = |cx: f32, cy: f32, start: f32| {
            (
                Piece::Arc {
                    centre: (x + cx, y + cy),
                    start,
                },
                quarter,
            )
        };
        use std::f32::consts::{FRAC_PI_2, PI};
        [
            line((x + w / 2., y), (x + w - r, y)),
            arc(w - r, r, -FRAC_PI_2),
            line((x + w, y + r), (x + w, y + h - r)),
            arc(w - r, h - r, 0.),
            line((x + w - r, y + h), (x + r, y + h)),
            arc(r, h - r, FRAC_PI_2),
            line((x, y + h - r), (x, y + r)),
            arc(r, r, PI),
            line((x + r, y), (x + w / 2., y)),
        ]
    }

    pub(crate) fn length(&self) -> f32 {
        self.pieces().iter().map(|(_, len)| len).sum()
    }

    /// The point at `s` (0 to 1, wrapped) along the outline.
    pub(crate) fn point(&self, s: f32) -> (f32, f32) {
        let pieces = self.pieces();
        let total: f32 = pieces.iter().map(|(_, len)| len).sum();
        let mut left = s.rem_euclid(1.) * total;
        for (piece, len) in &pieces {
            if left <= *len || *len == 0. && left == 0. {
                let u = if *len > 0. { left / len } else { 0. };
                return match piece {
                    Piece::Line { from, to } => {
                        (from.0 + (to.0 - from.0) * u, from.1 + (to.1 - from.1) * u)
                    }
                    Piece::Arc { centre, start } => {
                        let a = start + u * std::f32::consts::FRAC_PI_2;
                        (centre.0 + self.r * a.cos(), centre.1 + self.r * a.sin())
                    }
                };
            }
            left -= len;
        }
        (self.x + self.w / 2., self.y)
    }

    /// Points from `from` along the outline for `share` of it (0 to 1),
    /// about `step` points apart.
    pub(crate) fn points(&self, from: f32, share: f32, step: f32) -> Vec<(f32, f32)> {
        let total = self.length();
        if total <= 0. {
            return Vec::new();
        }
        let share = share.clamp(0., 1.);
        let n = ((share * total / step.max(0.5)).ceil() as usize).max(1);
        (0..=n)
            .map(|i| self.point(from + share * i as f32 / n as f32))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn running() -> Activity {
        Activity {
            phase: Phase::Running,
            sparks: 0,
        }
    }

    fn phase(phase: Phase) -> Activity {
        Activity { phase, sparks: 0 }
    }

    /// An edge and a clock in seconds.
    struct Rig {
        edge: Edge,
        origin: Instant,
    }

    impl Rig {
        fn new() -> Self {
            let origin = Instant::now();
            Self {
                edge: Edge::new(origin),
                origin,
            }
        }

        fn at(&self, t: f64) -> Instant {
            self.origin + Duration::from_secs_f64(t)
        }

        fn update(&mut self, t: f64, activity: Activity, rate: f32) -> Update {
            let now = self.at(t);
            self.edge.update(now, activity, rate, true)
        }

        fn still(&mut self, t: f64, activity: Activity) -> Update {
            let now = self.at(t);
            self.edge.update(now, activity, 0., false)
        }

        fn frame(&self, t: f64) -> EdgeFrame {
            frame_at(self.edge.state(), t)
        }

        fn show(&self) -> Show {
            self.edge.state().part.show
        }

        /// A run that has been travelling since `0.2`, until `until`.
        fn run(&mut self, until: f64) {
            let mut t = 0.;
            while t <= until {
                self.update(t, running(), 0.);
                t += 0.05;
            }
            assert_eq!(self.show(), Show::Travel);
        }
    }

    fn head_gap(a: f32, b: f32) -> f32 {
        let d = (a - b).abs();
        d.min(1. - d)
    }

    #[test]
    fn the_position_never_jumps_when_the_lap_time_changes() {
        let mut rig = Rig::new();
        rig.run(1.);
        let lap = rig.edge.state().part.lap;
        let before = rig.frame(1.5);
        let update = rig.update(1.5, running(), 1.);
        assert!(update.changed);
        assert!(rig.edge.state().part.lap < lap, "faster");
        let after = rig.frame(1.5);
        assert!(head_gap(before.head, after.head) < 1e-5);
        // And it now moves faster.
        let slow = head_gap(
            before.head,
            frame_at(
                &EdgeState {
                    ..*rig.edge.state()
                },
                1.6,
            )
            .head,
        );
        assert!(slow > 0.1 / MOTION.lap_slow as f32);
    }

    #[test]
    fn the_lap_time_changes_at_most_four_times_a_second() {
        let mut rig = Rig::new();
        rig.run(0.5);
        let mut changes = 0;
        let mut last = rig.edge.state().part.lap;
        let mut t = 0.5;
        for i in 0..40 {
            t += 0.05;
            rig.update(t, running(), if i % 2 == 0 { 1. } else { 0. });
            let lap = rig.edge.state().part.lap;
            if lap != last {
                changes += 1;
                last = lap;
            }
        }
        // 2 seconds.
        assert!(changes <= 8, "{changes} changes");
        assert!(changes >= 4, "it does follow the rate, {changes} changes");
    }

    #[test]
    fn a_spark_flares_once() {
        let mut rig = Rig::new();
        rig.run(1.);
        assert_eq!(rig.frame(1.).flare, 0.);
        rig.update(
            1.05,
            Activity {
                phase: Phase::Running,
                sparks: 1,
            },
            0.,
        );
        assert!(rig.frame(1.05 + MOTION.flare / 2.).flare > 0.9);
        assert_eq!(rig.frame(1.05 + MOTION.flare + 0.01).flare, 0.);
        // The same count again is no new flare.
        rig.update(
            1.5,
            Activity {
                phase: Phase::Running,
                sparks: 1,
            },
            0.,
        );
        assert_eq!(rig.frame(1.6).flare, 0.);
    }

    #[test]
    fn waiting_stops_the_travel_and_breathes_in_warn() {
        let mut rig = Rig::new();
        rig.run(1.);
        assert!(rig.update(1.05, phase(Phase::Waiting), 0.).changed);
        assert_eq!(rig.show(), Show::Breathe);
        let settled = 1.05 + MOTION.fade;
        let a = rig.frame(settled);
        let b = rig.frame(settled + MOTION.breath / 2.);
        for f in [a, b] {
            assert_eq!(f.highlight, 0.);
            assert_eq!(f.line_role, Role::Warn);
        }
        assert!((a.line - b.line).abs() > 0.2, "it breathes");
    }

    #[test]
    fn an_error_pulses_once_in_danger_and_ends() {
        let mut rig = Rig::new();
        rig.run(1.);
        rig.update(1.05, phase(Phase::Error), 0.);
        assert_eq!(rig.show(), Show::Pulse);
        let peak = rig.frame(1.05 + MOTION.pulse / 2.);
        assert_eq!(peak.line_role, Role::Danger);
        assert!(peak.line > 0.9);
        assert_eq!(rig.frame(1.05 + MOTION.pulse + 0.01), EdgeFrame::default());
        let end = rig.update(1.05 + MOTION.pulse + 0.01, phase(Phase::Error), 0.);
        assert!(end.settle);
        assert_eq!(rig.show(), Show::Off);
    }

    #[test]
    fn a_clean_end_finishes_the_lap_fades_and_settles() {
        let mut rig = Rig::new();
        rig.run(2.);
        let end = 2.05;
        let at = rig.frame(end).head;
        assert!(!rig.update(end, phase(Phase::Done), 0.).settle);
        assert_eq!(rig.show(), Show::Finish);
        let (run_end, _) = rig.edge.state().part.finish_target();
        assert!(run_end - end <= MOTION.finish_run + 1e-9);
        // It goes on from where it was, forward, to the end of the lap.
        assert!(head_gap(rig.frame(end).head, at) < 1e-5);
        let mut last = at;
        let mut t = end;
        while t < run_end {
            let head = rig.frame(t).head;
            assert!(head >= last || head < 0.05, "forward");
            last = head;
            t += 0.01;
        }
        // Then the whole edge lights and fades.
        let lit = rig.frame(run_end + MOTION.finish * 0.25);
        assert!(lit.line > 0.9 && lit.halo > 0.9);
        assert_eq!(
            rig.frame(run_end + MOTION.finish + 0.01),
            EdgeFrame::default()
        );
        assert!(!rig.update(run_end, phase(Phase::Done), 0.).settle);
        assert!(
            rig.update(run_end + MOTION.finish, phase(Phase::Done), 0.)
                .settle
        );
        assert_eq!(rig.show(), Show::Off);
    }

    #[test]
    fn a_run_shorter_than_the_delay_shows_nothing() {
        let mut rig = Rig::new();
        rig.update(0., running(), 0.);
        rig.update(0.1, running(), 0.);
        assert_eq!(rig.show(), Show::Off);
        assert_eq!(rig.frame(0.1), EdgeFrame::default());
        let end = rig.update(0.12, phase(Phase::Done), 0.);
        assert!(end.settle);
        assert_eq!(rig.frame(0.2), EdgeFrame::default());
        // An error that quick shows nothing either.
        let mut rig = Rig::new();
        rig.update(0., running(), 0.);
        assert!(rig.update(0.1, phase(Phase::Error), 0.).settle);
        assert_eq!(rig.frame(0.3), EdgeFrame::default());
    }

    #[test]
    fn the_light_appears_after_the_delay_and_says_when() {
        let mut rig = Rig::new();
        rig.update(0., running(), 0.);
        let wake = rig.edge.next_wake(rig.at(0.)).expect("a wake");
        assert!((wake - MOTION.delay).abs() < 1e-9);
        rig.update(wake, running(), 0.);
        assert_eq!(rig.show(), Show::Travel);
    }

    #[test]
    fn a_run_under_a_second_ends_without_the_lap() {
        let mut rig = Rig::new();
        rig.run(0.5);
        let end = rig.update(0.55, phase(Phase::Done), 0.);
        assert!(end.settle);
        assert_eq!(rig.show(), Show::Off);
        // A fade, not a cut.
        assert!(rig.frame(0.55 + MOTION.fade / 3.).highlight > 0.);
        assert_eq!(rig.frame(0.55 + MOTION.fade), EdgeFrame::default());
    }

    #[test]
    fn showing_another_session_goes_straight_off() {
        let mut rig = Rig::new();
        rig.run(2.);
        rig.update(2.05, phase(Phase::Idle), 0.);
        assert_eq!(rig.frame(2.05), EdgeFrame::default());
        assert!(!rig.edge.state().visible(2.05));
    }

    #[test]
    fn reduced_motion_is_still_while_running_and_waiting_and_off_at_the_end() {
        for live in [Phase::Running, Phase::Waiting] {
            let mut rig = Rig::new();
            rig.still(0., phase(live));
            rig.still(0.5, phase(live));
            let state = *rig.edge.state();
            assert!(!state.animating(1.));
            let (a, b) = (frame_at(&state, 1.), frame_at(&state, 3.));
            assert_eq!(a, b, "{live:?} does not move");
            assert_eq!(a.highlight, 0.);
            assert!(a.line > 0.5, "but it is lit");
            for end in [Phase::Error, Phase::Done] {
                let mut rig = Rig::new();
                rig.still(0., phase(live));
                rig.still(0.5, phase(live));
                assert!(rig.still(0.6, phase(end)).settle, "{live:?} then {end:?}");
                assert_eq!(rig.frame(0.6), EdgeFrame::default());
            }
        }
    }

    #[test]
    fn the_head_stays_on_the_perimeter_and_strengths_in_range() {
        let mut rig = Rig::new();
        let script = [
            (running(), 0.),
            (running(), 1.),
            (
                Activity {
                    phase: Phase::Running,
                    sparks: 3,
                },
                0.5,
            ),
            (phase(Phase::Waiting), 0.),
            (running(), 0.2),
            (phase(Phase::Done), 0.),
            (phase(Phase::Error), 0.),
        ];
        let mut t = 0.;
        for (activity, rate) in script {
            for _ in 0..40 {
                t += 0.037;
                rig.update(t, activity, rate);
                let f = rig.frame(t);
                assert!((0. ..1.).contains(&f.head), "{f:?}");
                for v in [f.tail, f.highlight, f.flare, f.glow, f.line, f.halo] {
                    assert!((0. ..=1.).contains(&v), "{f:?}");
                }
            }
        }
    }

    #[test]
    fn the_outline_is_measured_from_the_top_middle_clockwise() {
        let o = Outline::new(200., 100., 20.);
        let quarter = std::f32::consts::FRAC_PI_2 * 20.;
        let expected = 2. * (200. - 40.) + 2. * (100. - 40.) + 4. * quarter;
        assert!((o.length() - expected).abs() < 1e-3);
        let (x, y) = o.point(0.);
        assert!((x - 100.).abs() < 1e-3 && y.abs() < 1e-3);
        // A quarter of the way round is on the right edge, going down.
        let (x, y) = o.point(0.25);
        assert!((x - 200.).abs() < 1e-2 && y > 20. && y < 80., "({x}, {y})");
        // Consecutive points are close: no jumps, also across the start.
        let points = o.points(0.9, 0.2, 2.);
        for pair in points.windows(2) {
            let d = ((pair[1].0 - pair[0].0).powi(2) + (pair[1].1 - pair[0].1).powi(2)).sqrt();
            assert!(d < 3., "{d}");
        }
        // Inset keeps the corners concentric.
        let inner = o.inset(5.);
        assert_eq!(
            (inner.x, inner.y, inner.w, inner.h, inner.r),
            (5., 5., 190., 90., 15.)
        );
    }
}
