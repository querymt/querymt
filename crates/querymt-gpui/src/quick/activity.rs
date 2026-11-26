//! What the panel shows about the run of the session it shows (design D14).
//!
//! Pure logic, built on every desktop platform so that its tests run
//! everywhere: the panel reads the session into an [`Observation`], a
//! [`Tracker`] turns the observations into an [`Activity`], which the edge
//! light (`super::edge`) shows. Nothing here draws.
//!
//! The session has no "done" or "error" phase (its runtime phase is one of
//! `idle`, `starting`, `model`, `tools`, `waiting`), so Done and Error are
//! **transitions**: they are raised only by a run that ends while its session
//! is shown. Showing another session, or a new quick chat, takes that
//! session's state as it is: no finishing lap, and no pulse for an error it
//! already had.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use crate::session::open::SessionKey;
use crate::session::{ItemKind, SessionView, WorkEntry};

/// The time over which the stream rate is smoothed.
const SMOOTHING: Duration = Duration::from_millis(500);

/// Characters a second that count as a fast stream: the rate is 1 from here
/// on. About 100 tokens a second.
const FAST_STREAM: f32 = 400.;

/// What the shown session is doing, as far as the panel's edge is concerned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Nothing to show: no session, no run, or another session was just shown.
    #[default]
    Idle,
    /// The agent streams, runs a tool, or a prompt is in flight.
    Running,
    /// The agent waits for the user.
    Waiting,
    /// A run ended with an error it did not have before: one pulse.
    Error,
    /// A run ended cleanly: the finishing lap.
    Done,
}

impl Phase {
    /// A signal that ends by itself.
    pub(crate) fn is_transient(self) -> bool {
        matches!(self, Phase::Error | Phase::Done)
    }
}

/// What the effect shows. The stream rate is separate ([`Tracker::rate`]): it
/// decays with time, not with changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Activity {
    pub(crate) phase: Phase,
    /// Goes up whenever a tool call starts or ends; each step is one spark.
    pub(crate) sparks: u32,
}

/// The runtime phase of a session, as the projection names it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Runtime {
    #[default]
    Idle,
    Starting,
    Model,
    Tools,
    Waiting,
    /// Any other phase an agent reports: a run, as `SessionView::running`
    /// counts every phase that is not idle.
    Other,
}

impl Runtime {
    pub(crate) fn parse(phase: &str) -> Self {
        match phase {
            "" | "idle" => Runtime::Idle,
            "starting" => Runtime::Starting,
            "model" => Runtime::Model,
            "tools" => Runtime::Tools,
            "waiting" => Runtime::Waiting,
            _ => Runtime::Other,
        }
    }
}

/// The text being streamed: which part it is and how long it is. Reading it is
/// a length, not a scan (see [`observe`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stream {
    /// Identity of the reply or reasoning part. A different one is a new part.
    pub(crate) part: u64,
    /// Its length in bytes so far.
    pub(crate) len: usize,
}

/// What the panel reads from the session it shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Observation {
    /// The session shown; `None` for a new quick chat.
    pub(crate) session: Option<SessionKey>,
    pub(crate) runtime: Runtime,
    /// A prompt sent from this client is in flight.
    pub(crate) prompting: bool,
    /// A request of this session waits for the user's answer.
    pub(crate) request_pending: bool,
    /// Errors the session shows for its latest turn.
    pub(crate) errors: usize,
    /// Tool calls of the latest turn, and how many of them run.
    pub(crate) tools: usize,
    pub(crate) tools_running: usize,
    pub(crate) stream: Option<Stream>,
}

impl Observation {
    /// The agent waits for the user.
    fn waiting(&self) -> bool {
        self.runtime == Runtime::Waiting || self.request_pending
    }

    /// There is a run to show, or a question to answer.
    fn busy(&self) -> bool {
        self.session.is_some()
            && (self.prompting || self.runtime != Runtime::Idle || self.request_pending)
    }

    fn live_phase(&self) -> Phase {
        if !self.busy() {
            Phase::Idle
        } else if self.waiting() {
            Phase::Waiting
        } else {
            Phase::Running
        }
    }
}

fn hash_of(parts: &[&str]) -> u64 {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    hasher.finish()
}

/// Read `view`, the session `key`, into an observation. `request_pending`: the
/// inbox holds an open request of it.
///
/// Only the latest turn is looked at (from the end back to the last prompt of
/// the user), so the cost follows the turn, not the transcript, and the stream
/// length is one `String::len`: nothing is added to the projection.
pub(crate) fn observe(view: &SessionView, key: &SessionKey, request_pending: bool) -> Observation {
    let mut observation = Observation {
        session: Some(key.clone()),
        runtime: Runtime::parse(&view.phase),
        prompting: view.prompting,
        request_pending,
        errors: usize::from(view.error.is_some()),
        ..Observation::default()
    };
    let mut stream_seen = false;
    for item in view.items.iter().rev() {
        match &item.kind {
            ItemKind::User { .. } => break,
            ItemKind::Notice { error: true, .. } => observation.errors += 1,
            ItemKind::Agent { text } if !stream_seen => {
                stream_seen = true;
                observation.stream = Some(Stream {
                    part: hash_of(&[&item.key]),
                    len: text.len(),
                });
            }
            ItemKind::Work(entries) => {
                for entry in entries.iter().rev() {
                    match entry {
                        WorkEntry::Tool(call) => {
                            observation.tools += 1;
                            observation.tools_running += usize::from(call.running());
                        }
                        WorkEntry::Thought(thought) if !stream_seen => {
                            stream_seen = true;
                            observation.stream = Some(Stream {
                                part: hash_of(&[&item.key, &thought.id]),
                                len: thought.text.len(),
                            });
                        }
                        WorkEntry::Thought(_) => {}
                    }
                }
            }
            _ => {}
        }
    }
    observation
}

/// A leaky count of what streamed: it gains what is appended and loses
/// `exp(-dt / SMOOTHING)` of itself over `dt`, so a steady stream of `r`
/// characters a second settles at `r * SMOOTHING`.
#[derive(Clone, Copy, Debug)]
struct Meter {
    level: f32,
    at: Instant,
}

impl Meter {
    fn decayed(&self, now: Instant) -> f32 {
        let dt = now.saturating_duration_since(self.at);
        self.level * (-dt.as_secs_f32() / SMOOTHING.as_secs_f32()).exp()
    }

    fn add(&mut self, appended: usize, now: Instant) {
        self.level = self.decayed(now) + appended as f32;
        self.at = self.at.max(now);
    }

    /// Scaled so that [`FAST_STREAM`] characters a second are 1.
    fn rate(&self, now: Instant) -> f32 {
        (self.decayed(now) / SMOOTHING.as_secs_f32() / FAST_STREAM).clamp(0., 1.)
    }
}

/// Turns what the panel reads into what it shows.
#[derive(Debug)]
pub(crate) struct Tracker {
    /// The last observation: what the next one is compared with. `None`
    /// before the first.
    last: Option<Observation>,
    activity: Activity,
    meter: Meter,
}

impl Tracker {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last: None,
            activity: Activity::default(),
            meter: Meter { level: 0., at: now },
        }
    }

    #[cfg(test)]
    pub(crate) fn activity(&self) -> Activity {
        self.activity
    }

    /// How fast text streams, from 0 to 1. It falls back by itself when the
    /// text stops, so it is read at each frame, not at each change.
    pub(crate) fn rate(&self, now: Instant) -> f32 {
        self.meter.rate(now)
    }

    /// The edge has shown the end of the run (its finishing lap or pulse).
    pub(crate) fn settle(&mut self) {
        if self.activity.phase.is_transient() {
            self.activity.phase = Phase::Idle;
        }
    }

    /// Take what the panel reads now.
    pub(crate) fn observe(&mut self, now: Instant, observation: Observation) -> Activity {
        let same_session = self
            .last
            .as_ref()
            .is_some_and(|last| last.session == observation.session);
        let Some(last) = self.last.take().filter(|_| same_session) else {
            // Another session, or the first one: take its state as it is.
            self.meter = Meter { level: 0., at: now };
            self.activity.phase = observation.live_phase();
            self.last = Some(observation);
            return self.activity;
        };

        // The rate: what was appended to the part being streamed. A new part
        // starts empty, so all of its text is new.
        if let Some(stream) = observation.stream {
            let appended = match last.stream {
                Some(before) if before.part == stream.part => stream.len.saturating_sub(before.len),
                _ => stream.len,
            };
            if appended > 0 {
                self.meter.add(appended, now);
            }
        }

        // A spark each time a tool call starts or ends during a run.
        let tools_moved =
            (last.tools, last.tools_running) != (observation.tools, observation.tools_running);
        if tools_moved && (last.busy() || observation.busy()) {
            self.activity.sparks = self.activity.sparks.wrapping_add(1);
        }

        let failed_now = observation.errors > last.errors;
        self.activity.phase = if observation.busy() {
            observation.live_phase()
        } else if last.busy() {
            // The run ended while this session is shown.
            if failed_now {
                Phase::Error
            } else {
                Phase::Done
            }
        } else if self.activity.phase == Phase::Done && failed_now {
            // The error came a moment after the end.
            Phase::Error
        } else {
            // The end stays until the edge has shown it.
            match self.activity.phase {
                phase if phase.is_transient() => phase,
                _ => Phase::Idle,
            }
        };
        self.last = Some(observation);
        self.activity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(session: &str) -> Option<SessionKey> {
        Some(SessionKey::new("agent", session))
    }

    fn idle(session: &str) -> Observation {
        Observation {
            session: key(session),
            ..Observation::default()
        }
    }

    fn running(session: &str) -> Observation {
        Observation {
            runtime: Runtime::Model,
            ..idle(session)
        }
    }

    fn ms(clock: Instant, ms: u64) -> Instant {
        clock + Duration::from_millis(ms)
    }

    // ---- the phase table -------------------------------------------------

    #[test]
    fn every_phase_but_idle_is_a_run() {
        for phase in ["starting", "model", "tools", "waiting", "something-new"] {
            assert_ne!(Runtime::parse(phase), Runtime::Idle, "{phase}");
        }
        assert_eq!(Runtime::parse(""), Runtime::Idle);
        assert_eq!(Runtime::parse("idle"), Runtime::Idle);
    }

    #[test]
    fn idle_to_running() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        assert_eq!(tracker.observe(t0, idle("a")).phase, Phase::Idle);
        let phase = tracker.observe(ms(t0, 10), running("a")).phase;
        assert_eq!(phase, Phase::Running);
    }

    #[test]
    fn a_prompt_in_flight_is_running_before_the_agent_says_so() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        let sent = Observation {
            prompting: true,
            ..idle("a")
        };
        assert_eq!(tracker.observe(ms(t0, 1), sent).phase, Phase::Running);
    }

    #[test]
    fn a_clean_end_is_done_until_settled() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        tracker.observe(ms(t0, 10), running("a"));
        let ended = tracker.observe(ms(t0, 20), idle("a")).phase;
        assert_eq!(ended, Phase::Done);
        // Further changes keep it until the edge has shown its end.
        assert_eq!(tracker.observe(ms(t0, 30), idle("a")).phase, Phase::Done);
        tracker.settle();
        assert_eq!(tracker.activity().phase, Phase::Idle);
    }

    #[test]
    fn an_end_with_a_new_error_is_an_error_until_settled() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        tracker.observe(ms(t0, 10), running("a"));
        let failed = Observation {
            errors: 1,
            ..idle("a")
        };
        let phase = tracker.observe(ms(t0, 20), failed).phase;
        assert_eq!(phase, Phase::Error);
        tracker.settle();
        assert_eq!(tracker.activity().phase, Phase::Idle);
    }

    #[test]
    fn an_error_that_comes_just_after_the_end_turns_it_into_an_error() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        tracker.observe(ms(t0, 10), running("a"));
        assert_eq!(tracker.observe(ms(t0, 20), idle("a")).phase, Phase::Done);
        let failed = Observation {
            errors: 1,
            ..idle("a")
        };
        assert_eq!(tracker.observe(ms(t0, 30), failed).phase, Phase::Error);
    }

    #[test]
    fn waiting_is_its_own_phase() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        let waiting = Observation {
            runtime: Runtime::Waiting,
            ..idle("a")
        };
        let phase = tracker.observe(ms(t0, 10), waiting).phase;
        assert_eq!(phase, Phase::Waiting);
    }

    #[test]
    fn a_pending_request_is_waiting_even_while_the_runtime_says_tools() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        let asking = Observation {
            runtime: Runtime::Tools,
            request_pending: true,
            ..idle("a")
        };
        assert_eq!(tracker.observe(ms(t0, 10), asking).phase, Phase::Waiting);
    }

    #[test]
    fn an_idle_panel_stays_idle() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        for step in 0..20 {
            let observation = if step % 2 == 0 {
                idle("a")
            } else {
                Observation::default()
            };
            let phase = tracker.observe(ms(t0, step * 16), observation).phase;
            assert_eq!(phase, Phase::Idle);
        }
    }

    #[test]
    fn switching_session_neither_finishes_nor_pulses() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        tracker.observe(ms(t0, 10), running("a"));
        // The shown session is running, and then an idle one is shown: off at
        // once, with no end of turn.
        assert_eq!(tracker.observe(ms(t0, 20), idle("b")).phase, Phase::Idle);
        // A new quick chat does the same.
        tracker.observe(ms(t0, 30), running("b"));
        assert_eq!(
            tracker.observe(ms(t0, 40), Observation::default()).phase,
            Phase::Idle
        );
        // And a running session shown from idle is running at once, with no
        // spark for what it was already doing.
        let before = tracker.activity().sparks;
        let shown = Observation {
            tools: 3,
            tools_running: 1,
            ..running("c")
        };
        assert_eq!(tracker.observe(ms(t0, 50), shown).phase, Phase::Running);
        assert_eq!(tracker.activity().sparks, before);
    }

    #[test]
    fn an_error_the_session_already_had_does_not_pulse() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        let had_error = |runtime| Observation {
            errors: 1,
            runtime,
            ..idle("a")
        };
        // Shown with an error already on it, then a clean run ends.
        tracker.observe(t0, had_error(Runtime::Idle));
        tracker.observe(ms(t0, 10), had_error(Runtime::Model));
        assert_eq!(
            tracker.observe(ms(t0, 20), had_error(Runtime::Idle)).phase,
            Phase::Done
        );
        tracker.settle();
        // Shown again later: no pulse either.
        tracker.observe(ms(t0, 30), idle("b"));
        assert_eq!(
            tracker.observe(ms(t0, 40), had_error(Runtime::Idle)).phase,
            Phase::Idle
        );
    }

    // ---- reading a real session -------------------------------------------

    fn chunk(view: &mut SessionView, kind: &str, text: &str) {
        view.session_update(&serde_json::json!({
            "sessionId": "s",
            "update": {
                "sessionUpdate": kind,
                "messageId": "m1",
                "content": { "type": "text", "text": text },
            },
        }));
    }

    #[test]
    fn the_stream_length_follows_the_reply_and_the_reasoning_being_streamed() {
        let key = SessionKey::new("agent", "s");
        let mut view = SessionView::new("agent", "s", "/w", "S");
        assert_eq!(observe(&view, &key, false).stream, None);

        chunk(&mut view, "agent_message_chunk", "Hello");
        let first = observe(&view, &key, false).stream.expect("a reply streams");
        assert_eq!(first.len, 5);
        chunk(&mut view, "agent_message_chunk", ", world");
        let second = observe(&view, &key, false).stream.expect("a reply streams");
        assert_eq!(second.part, first.part, "the same reply");
        assert_eq!(second.len, 12);

        // Reasoning before any reply is a part of its own.
        let mut view = SessionView::new("agent", "s", "/w", "S");
        chunk(&mut view, "agent_thought_chunk", "hmm");
        let thought = observe(&view, &key, false)
            .stream
            .expect("reasoning streams");
        assert_ne!(thought.part, first.part);
        assert_eq!(thought.len, 3);
        chunk(&mut view, "agent_thought_chunk", " ok");
        let more = observe(&view, &key, false)
            .stream
            .expect("reasoning streams");
        assert_eq!((more.part, more.len), (thought.part, 6));
    }

    #[test]
    fn the_phase_and_the_prompt_of_the_session_are_read() {
        let key = SessionKey::new("agent", "s");
        let mut view = SessionView::new("agent", "s", "/w", "S");
        let idle = observe(&view, &key, false);
        assert_eq!((idle.runtime, idle.prompting), (Runtime::Idle, false));
        view.phase = "tools".into();
        view.prompting = true;
        let busy = observe(&view, &key, true);
        assert_eq!((busy.runtime, busy.prompting), (Runtime::Tools, true));
        assert!(busy.request_pending);
        assert_eq!(busy.session, Some(key));
    }

    // ---- sparks ----------------------------------------------------------

    #[test]
    fn a_tool_call_starting_or_ending_is_one_spark() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, running("a"));
        let tool = |tools, tools_running| Observation {
            tools,
            tools_running,
            ..running("a")
        };
        tracker.observe(ms(t0, 10), tool(1, 1));
        assert_eq!(tracker.activity().sparks, 1, "started");
        tracker.observe(ms(t0, 20), tool(1, 1));
        assert_eq!(tracker.activity().sparks, 1, "nothing changed");
        tracker.observe(ms(t0, 30), tool(1, 0));
        assert_eq!(tracker.activity().sparks, 2, "ended");
    }

    #[test]
    fn tool_calls_of_an_idle_session_are_no_spark() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, idle("a"));
        let older = Observation {
            tools: 4,
            ..idle("a")
        };
        tracker.observe(ms(t0, 10), older);
        assert_eq!(tracker.activity().sparks, 0);
    }

    // ---- the stream rate -------------------------------------------------

    fn streaming(len: usize) -> Observation {
        Observation {
            stream: Some(Stream { part: 1, len }),
            ..running("a")
        }
    }

    /// Stream `per_second` characters a second for `seconds`, in 50 ms
    /// steps; the rate right after.
    fn rate_after(per_second: usize, seconds: u64) -> f32 {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, streaming(0));
        let (mut len, mut at) = (0, 0);
        while at < seconds * 1000 {
            at += 50;
            len += per_second / 20;
            tracker.observe(ms(t0, at), streaming(len));
        }
        tracker.rate(ms(t0, at))
    }

    #[test]
    fn the_rate_rises_with_faster_text() {
        let slow = rate_after(40, 2);
        let medium = rate_after(160, 2);
        let fast = rate_after(400, 2);
        assert!(
            0. < slow && slow < medium && medium < fast,
            "{slow} {medium} {fast}"
        );
        assert!(slow < 0.2, "{slow}");
        assert!(fast > 0.9, "{fast}");
    }

    #[test]
    fn the_rate_falls_back_when_the_text_stops() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, streaming(0));
        let mut len = 0;
        for step in 1..=40 {
            // 400 characters a second: a fast stream, a rate near 1.
            len += 20;
            tracker.observe(ms(t0, step * 50), streaming(len));
        }
        let at_end = ms(t0, 2000);
        let during = tracker.rate(at_end);
        assert!(during > 0.5);
        let later = tracker.rate(ms(t0, 2500));
        let much_later = tracker.rate(ms(t0, 5000));
        assert!(later < during * 0.5, "{during} {later}");
        assert!(much_later < 0.01, "{much_later}");
    }

    #[test]
    fn the_rate_stays_between_zero_and_one() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        tracker.observe(t0, streaming(0));
        assert_eq!(tracker.rate(t0), 0.);
        // One huge chunk, and then the clock standing still or going back.
        tracker.observe(ms(t0, 1), streaming(1_000_000));
        assert_eq!(tracker.rate(ms(t0, 1)), 1.);
        assert!((0. ..=1.).contains(&tracker.rate(t0)));
        assert!((0. ..=1.).contains(&tracker.rate(ms(t0, 60_000))));
    }

    #[test]
    fn a_new_part_counts_all_its_text_and_showing_a_session_counts_none() {
        let t0 = Instant::now();
        let mut tracker = Tracker::new(t0);
        // The session is shown with a long reply already in it.
        tracker.observe(t0, streaming(50_000));
        assert_eq!(tracker.rate(ms(t0, 1)), 0.);
        // A new part starts: its text is new.
        let next = Observation {
            stream: Some(Stream { part: 2, len: 80 }),
            ..running("a")
        };
        tracker.observe(ms(t0, 100), next);
        assert!(tracker.rate(ms(t0, 100)) > 0.);
        // The part getting shorter (a rebuilt row) is not negative text.
        let shorter = Observation {
            stream: Some(Stream { part: 2, len: 10 }),
            ..running("a")
        };
        let before = tracker.rate(ms(t0, 100));
        tracker.observe(ms(t0, 100), shorter);
        assert_eq!(tracker.rate(ms(t0, 100)), before);
    }
}
