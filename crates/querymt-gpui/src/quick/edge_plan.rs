//! What a renderer that animates by itself needs to know (design D14): the
//! light's [`EdgeState`] turned into keyframe tracks and head legs that a
//! compositor can run without the main thread.
//!
//! Pure, built on every platform. The tracks are [`frame_at`] sampled over
//! the stretch where something changes (a cross-fade, a flare, a pulse, a
//! finish), then either a hold or, for a breath, one repeating period; the
//! head runs on legs that follow [`Part::position`]. Both therefore come
//! from the same state and the same constants as the ring GPUI draws, and
//! [`Plan::at`] evaluates a plan the way the compositor will, so the tests
//! can hold it to `frame_at` at sample times.

use super::edge::{EdgeFrame, EdgeState, MOTION, Part, Role, Show, frame_at};

/// Spacing of the samples of the finite stretch, in seconds. Linear
/// interpolation between them stays within a percent of the curves.
pub(crate) const STEP: f64 = 1. / 60.;
/// Samples in one period of a repeating breath.
const BREATH_SAMPLES: usize = 48;
/// How many pieces the highlight is drawn in: the head and two fainter
/// segments of its tail.
pub(crate) const PIECES: usize = 3;
/// How strong each piece is, tail first.
pub(crate) const PIECE_STRENGTH: [f32; PIECES] = [0.25, 0.55, 1.];

/// Everything a layer's properties are set from, at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Levels {
    /// Opacity of the line layer of each role (accent, warn, danger); only
    /// the role the frame names is lit.
    pub(crate) line: [f32; 3],
    /// `shadowOpacity` of each line layer: its glow is the layer's shadow,
    /// which is scaled by the layer's own opacity.
    pub(crate) line_glow: [f32; 3],
    /// Opacity of the highlight, before the strength of each piece.
    pub(crate) highlight: f32,
    /// Stroke width of the highlight: wider while it flares.
    pub(crate) width: f32,
    /// `shadowOpacity` of the highlight's pieces.
    pub(crate) glow: f32,
}

impl Levels {
    /// Nothing is lit.
    pub(crate) fn is_blank(&self) -> bool {
        self.line.iter().all(|l| *l <= 0.001) && self.highlight <= 0.001
    }
}

fn role_index(role: Role) -> usize {
    match role {
        Role::Accent => 0,
        Role::Warn => 1,
        Role::Danger => 2,
    }
}

/// The layer properties for `frame`.
pub(crate) fn levels(frame: &EdgeFrame) -> Levels {
    let mut out = Levels {
        highlight: frame.highlight,
        width: MOTION.line_width * (1. + frame.flare),
        glow: (frame.glow * (1. + frame.flare) * MOTION.glow_alpha).clamp(0., 1.),
        ..Levels::default()
    };
    let r = role_index(frame.line_role);
    out.line[r] = frame.line;
    // The glow is `halo * glow_alpha` in all, as the ring draws it; the
    // layer's opacity multiplies its shadow, so it is divided out.
    if frame.line > 0.001 {
        out.line_glow[r] = (frame.halo * MOTION.glow_alpha / frame.line).clamp(0., 1.);
    }
    out
}

/// Keyframes of every level over `duration` seconds from `begin`, linear
/// between them. `keys` are at 0 to 1 of the duration.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Track {
    pub(crate) begin: f64,
    pub(crate) duration: f64,
    pub(crate) repeat: bool,
    pub(crate) keys: Vec<(f32, Levels)>,
}

impl Track {
    /// The levels `dt` seconds from the plan's start, if this track is
    /// running then. The compositor does this for the real light.
    #[cfg(test)]
    fn at(&self, dt: f64) -> Option<Levels> {
        let mut local = dt - self.begin;
        if local < 0. {
            return None;
        }
        if self.repeat {
            local %= self.duration;
        } else if local > self.duration {
            return None;
        }
        let u = (local / self.duration) as f32;
        let (mut before, mut after) = (self.keys.first()?, self.keys.last()?);
        for pair in self.keys.windows(2) {
            if u >= pair[0].0 && u <= pair[1].0 {
                before = &pair[0];
                after = &pair[1];
                break;
            }
        }
        let span = (after.0 - before.0).max(f32::EPSILON);
        let f = ((u - before.0) / span).clamp(0., 1.);
        let mix = |a: f32, b: f32| a + (b - a) * f;
        let (a, b) = (&before.1, &after.1);
        Some(Levels {
            line: std::array::from_fn(|i| mix(a.line[i], b.line[i])),
            line_glow: std::array::from_fn(|i| mix(a.line_glow[i], b.line_glow[i])),
            highlight: mix(a.highlight, b.highlight),
            width: mix(a.width, b.width),
            glow: mix(a.glow, b.glow),
        })
    }
}

/// The head moving linearly from `from` to `to` (0 to 1 along the
/// perimeter) over `duration` seconds from `begin`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HeadLeg {
    pub(crate) begin: f64,
    pub(crate) duration: f64,
    pub(crate) from: f32,
    pub(crate) to: f32,
    pub(crate) repeat: bool,
}

/// Where the highlight's pieces start and end on a path that traces the
/// outline twice, for a head at `head` (0 to 1) of the perimeter. Piece `i`
/// of [`PIECES`] covers a `PIECES`th of the tail, the last piece ending at
/// the head. The second trace is what lets a tail cross the start of the
/// outline.
pub(crate) fn piece_range(head: f32, tail: f32, i: usize) -> (f32, f32) {
    let piece = tail / PIECES as f32;
    let start = (1. + head - tail + piece * i as f32) / 2.;
    (start, start + piece / 2.)
}

/// A state as animations that start `now` (on the edge's clock).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    /// The levels as tracks; a later track that is running wins.
    pub(crate) tracks: Vec<Track>,
    /// The head's legs, in order; the last that has begun wins.
    pub(crate) legs: Vec<HeadLeg>,
    /// What the levels and the head rest at once nothing runs any more.
    pub(crate) hold: Levels,
    pub(crate) hold_head: f32,
    pub(crate) tail: f32,
}

impl Plan {
    /// Nothing is lit and nothing will be: the renderer can hide itself.
    pub(crate) fn idle(&self) -> bool {
        self.hold.is_blank()
            && !self.tracks.iter().any(|t| t.repeat)
            && !self.legs.iter().any(|l| l.repeat)
    }

    /// The levels `dt` seconds on, as the compositor shows them.
    #[cfg(test)]
    pub(crate) fn levels_at(&self, dt: f64) -> Levels {
        self.tracks
            .iter()
            .rev()
            .find_map(|t| t.at(dt))
            .unwrap_or(self.hold)
    }

    /// The head `dt` seconds on, from 0 to 1.
    #[cfg(test)]
    pub(crate) fn head_at(&self, dt: f64) -> f32 {
        for leg in self.legs.iter().rev() {
            let mut local = dt - leg.begin;
            if local < 0. {
                continue;
            }
            if leg.repeat {
                local %= leg.duration;
            } else if local > leg.duration {
                continue;
            }
            let u = (local / leg.duration) as f32;
            return leg.from + (leg.to - leg.from) * u;
        }
        self.hold_head
    }

    /// The light `dt` seconds on, in the terms of [`EdgeFrame`] that a plan
    /// can say: for tests against [`frame_at`].
    #[cfg(test)]
    pub(crate) fn at(&self, dt: f64) -> (Levels, f32) {
        (self.levels_at(dt), self.head_at(dt))
    }

    /// The plan for `state` at `now`, in seconds on the edge's clock.
    pub(crate) fn new(state: &EdgeState, now: f64) -> Self {
        let mut end = now;
        if let Some((_, at)) = state.prev {
            end = end.max(at + MOTION.fade);
        }
        if let Some(at) = state.part.flare_at {
            end = end.max(at + MOTION.flare);
        }
        if let Some(over) = state.part.ends_at() {
            end = end.max(over);
        }
        let span = end - now;
        let mut tracks = Vec::new();
        if span > 0. {
            let n = (span / STEP).ceil().max(1.) as usize;
            let keys = (0..=n)
                .map(|i| {
                    let u = i as f64 / n as f64;
                    (u as f32, levels(&frame_at(state, now + u * span)))
                })
                .collect();
            tracks.push(Track {
                begin: 0.,
                duration: span,
                repeat: false,
                keys,
            });
        }
        let mut hold = levels(&frame_at(state, end));
        if state.part.show == Show::Breathe && state.moving {
            // One period, repeated: the breath is periodic, so a period
            // sampled where the cross-fade ends is every later one.
            let keys: Vec<_> = (0..=BREATH_SAMPLES)
                .map(|i| {
                    let u = i as f64 / BREATH_SAMPLES as f64;
                    (u as f32, levels(&frame_at(state, end + u * MOTION.breath)))
                })
                .collect();
            hold = keys[0].1;
            tracks.push(Track {
                begin: span,
                duration: MOTION.breath,
                repeat: true,
                keys,
            });
        }
        let (legs, hold_head) = head_legs(state, now);
        Self {
            tracks,
            legs,
            hold,
            hold_head,
            tail: MOTION.tail,
        }
    }
}

/// The share of a lap `laps` is into it, from 0 up to but not 1.
fn wrap(laps: f64) -> f32 {
    let share = laps.rem_euclid(1.) as f32;
    if share >= 1. { 0. } else { share }
}

/// The part whose highlight is drawn, as [`frame_at`] picks it: the
/// current one when it has a highlight, else the one fading out.
fn highlight_source(state: &EdgeState, now: f64) -> Option<(Part, bool)> {
    let has = |part: &Part| match part.show {
        Show::Travel => state.moving,
        Show::Finish => true,
        _ => false,
    };
    if has(&state.part) {
        return Some((state.part, true));
    }
    state
        .prev
        .filter(|_| state.fading(now))
        .map(|(part, _)| part)
        .filter(has)
        .map(|part| (part, false))
}

fn head_legs(state: &EdgeState, now: f64) -> (Vec<HeadLeg>, f32) {
    let Some((part, current)) = highlight_source(state, now) else {
        return (Vec::new(), 1.);
    };
    match part.show {
        Show::Finish => {
            let (run_end, _) = part.finish_target();
            if now >= run_end {
                return (Vec::new(), 1.);
            }
            let from = wrap(part.position(now));
            (
                vec![HeadLeg {
                    begin: 0.,
                    duration: run_end - now,
                    from,
                    to: 1.,
                    repeat: false,
                }],
                1.,
            )
        }
        _ => {
            let from = wrap(part.position(now));
            let first = (1. - from as f64) * part.lap;
            let mut legs = Vec::new();
            if first > 0. {
                legs.push(HeadLeg {
                    begin: 0.,
                    duration: first,
                    from,
                    to: 1.,
                    repeat: false,
                });
            }
            // A part fading out only needs to move on while it fades, which
            // is shorter than a lap; the current one goes on for good.
            legs.push(HeadLeg {
                begin: first,
                duration: part.lap,
                from: 0.,
                to: 1.,
                repeat: current,
            });
            (legs, 1.)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quick::activity::{Activity, Phase};
    use crate::quick::edge::Edge;
    use std::time::{Duration, Instant};

    fn head_gap(a: f32, b: f32) -> f32 {
        let d = (a - b).abs();
        d.min(1. - d)
    }

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

        fn update(&mut self, t: f64, phase: Phase, sparks: u32, rate: f32, moving: bool) {
            let now = self.origin + Duration::from_secs_f64(t);
            self.edge
                .update(now, Activity { phase, sparks }, rate, moving);
        }

        /// A run travelling, stepped like the panel's renders.
        fn run(&mut self, until: f64) {
            let mut t = 0.;
            while t <= until {
                self.update(t, Phase::Running, 0, 0., true);
                t += 0.05;
            }
        }

        fn plan(&self, now: f64) -> Plan {
            Plan::new(self.edge.state(), now)
        }

        /// The plan at `now` must show what `frame_at` says at `now + dt`
        /// for every `dt` of `0..span`.
        fn check(&self, now: f64, span: f64) {
            let state = *self.edge.state();
            let plan = self.plan(now);
            let mut dt = 0.;
            while dt <= span {
                let f = frame_at(&state, now + dt);
                let (got, head) = plan.at(dt);
                let want = levels(&f);
                let close = |a: f32, b: f32, what: &str| {
                    assert!(
                        (a - b).abs() < 0.03,
                        "{what} at +{dt:.3}s: plan {a}, frame_at {b}"
                    )
                };
                // A change of colour is a step in `frame_at`, which the
                // linear samples turn into a ramp of one step; so the
                // strengths are compared summed over the colours, and each
                // colour away from a change.
                close(got.line.iter().sum(), want.line.iter().sum(), "line");
                // The glow itself, not the ratio that makes it.
                let glow = |l: &Levels| (0..3).map(|i| l.line[i] * l.line_glow[i]).sum::<f32>();
                close(glow(&got), glow(&want), "line glow");
                let role = |dt: f64| frame_at(&state, now + dt).line_role;
                if role(dt - 2. * STEP) == role(dt + 2. * STEP) {
                    for i in 0..3 {
                        close(got.line[i], want.line[i], "line of one colour");
                    }
                }
                close(got.highlight, want.highlight, "highlight");
                if f.highlight > 0.05 {
                    close(got.width, want.width, "width");
                    close(got.glow, want.glow, "glow");
                    let gap = head_gap(head, f.head);
                    assert!(
                        gap < 0.01,
                        "head at +{dt:.3}s: plan {head}, frame_at {}",
                        f.head
                    );
                }
                dt += 0.0137;
            }
        }
    }

    #[test]
    fn a_travel_follows_frame_at() {
        let mut rig = Rig::new();
        rig.run(1.);
        // Past several laps, through the wrap of the head.
        rig.check(1., 9.);
    }

    #[test]
    fn a_travel_runs_for_good_and_a_hold_does_not_animate() {
        let mut rig = Rig::new();
        rig.run(1.);
        let plan = rig.plan(1.);
        assert!(!plan.idle());
        assert!(plan.legs.iter().any(|l| l.repeat));
        let mut still = Rig::new();
        still.update(0., Phase::Running, 0, 0., false);
        still.update(0.2, Phase::Running, 0, 0., false);
        let plan = still.plan(0.2);
        assert!(plan.legs.is_empty(), "no head under reduced motion");
        assert!(plan.tracks.iter().all(|t| !t.repeat));
        assert!(!plan.hold.is_blank(), "a still, lit edge");
    }

    #[test]
    fn a_change_of_lap_time_keeps_the_head_where_it_is() {
        let mut rig = Rig::new();
        rig.run(1.);
        let before = frame_at(rig.edge.state(), 1.5).head;
        rig.update(1.5, Phase::Running, 0, 1., true);
        let plan = rig.plan(1.5);
        assert!(head_gap(plan.head_at(0.), before) < 1e-4);
        rig.check(1.5, 5.);
    }

    #[test]
    fn a_spark_flares_and_a_flare_ends_inside_the_plan() {
        let mut rig = Rig::new();
        rig.run(1.);
        rig.update(1.05, Phase::Running, 1, 0., true);
        let plan = rig.plan(1.05);
        let flared = plan.levels_at(MOTION.flare / 2.);
        assert!(flared.width > MOTION.line_width * 1.9);
        assert!((plan.levels_at(MOTION.flare + 0.05).width - MOTION.line_width).abs() < 1e-4);
        rig.check(1.05, 2.);
    }

    #[test]
    fn waiting_breathes_in_warn_and_repeats() {
        let mut rig = Rig::new();
        rig.run(1.);
        rig.update(1.05, Phase::Waiting, 0, 0., true);
        let plan = rig.plan(1.05);
        assert!(plan.tracks.iter().any(|t| t.repeat), "the breath repeats");
        assert!(plan.legs.iter().all(|l| !l.repeat), "the head stops");
        assert!(!plan.idle());
        // Over several periods, past the cross-fade.
        rig.check(1.05, MOTION.breath * 3.);
        let lit = plan.levels_at(0.5);
        assert!(lit.line[1] > 0. || plan.levels_at(1.5).line[1] > 0., "warn");
    }

    #[test]
    fn an_error_pulses_once_and_the_plan_ends_idle() {
        let mut rig = Rig::new();
        rig.run(1.);
        rig.update(1.05, Phase::Error, 0, 0., true);
        let plan = rig.plan(1.05);
        let peak = plan.levels_at(MOTION.pulse / 2.);
        assert!(peak.line[2] > 0.9, "danger");
        assert!(plan.hold.is_blank());
        assert!(plan.idle(), "nothing runs after the pulse");
        rig.check(1.05, 2. * MOTION.pulse + 0.5);
    }

    #[test]
    fn a_clean_end_finishes_the_lap_and_fades() {
        let mut rig = Rig::new();
        rig.run(2.);
        rig.update(2.05, Phase::Done, 0, 0., true);
        let plan = rig.plan(2.05);
        assert!(plan.idle());
        assert!(plan.legs.iter().all(|l| !l.repeat));
        rig.check(2.05, 3.);
    }

    #[test]
    fn a_waiting_run_that_goes_on_moves_the_head_through_the_fade() {
        let mut rig = Rig::new();
        rig.run(1.);
        rig.update(1.0, Phase::Idle, 0, 0., true);
        // The travel is off at once with nothing to fade, so no head.
        let plan = rig.plan(1.0);
        assert!(plan.idle());
        // A hand-built fade out of a travel into a breath keeps moving.
        rig.update(1.1, Phase::Running, 0, 0., true);
        rig.run(2.);
        rig.update(2.05, Phase::Waiting, 0, 0., true);
        let state = *rig.edge.state();
        assert!(state.prev.is_some());
        let plan = rig.plan(2.05);
        assert!(
            !plan.legs.is_empty(),
            "the old highlight moves while it fades"
        );
        assert!(plan.legs.iter().all(|l| !l.repeat));
        rig.check(2.05, MOTION.fade);
    }

    #[test]
    fn the_pieces_stay_on_the_doubled_path_and_cover_the_tail() {
        for h in [0., 0.01, 0.5, 0.99, 1.] {
            let mut covered = 0.;
            for i in 0..PIECES {
                let (s, e) = piece_range(h, MOTION.tail, i);
                assert!(
                    (0. ..=1.).contains(&s) && (0. ..=1.).contains(&e),
                    "{h}: {s} {e}"
                );
                covered += (e - s) * 2.;
            }
            assert!((covered - MOTION.tail).abs() < 1e-5);
        }
        // The head piece ends where the head is, on the second trace.
        let (_, e) = piece_range(0.25, MOTION.tail, PIECES - 1);
        assert!((e * 2. - 1. - 0.25).abs() < 1e-5);
    }

    #[test]
    fn an_off_state_is_idle() {
        let rig = Rig::new();
        let plan = rig.plan(0.);
        assert!(plan.idle());
        assert!(plan.tracks.is_empty() && plan.legs.is_empty());
    }
}
