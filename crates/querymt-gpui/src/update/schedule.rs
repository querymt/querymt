//! When update checks run, with an injected clock: the scheduler holds no
//! timers, it answers "is a check due?" for a given time since the app started.

use std::time::Duration;

/// The first automatic check comes this long after start.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
/// Automatic checks repeat this often while the app runs.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The longest the scheduler sleeps without looking at the settings.
pub const MAX_TICK: Duration = Duration::from_secs(60);

/// What starts a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The schedule.
    Auto,
    /// "Check now".
    Manual,
}

/// At most one check runs at a time.
#[derive(Clone, Debug, Default)]
pub struct Scheduler {
    in_flight: bool,
    /// Time since start at which the last check began.
    last_started: Option<Duration>,
}

impl Scheduler {
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Whether the schedule asks for a check `since_start` after the app
    /// started: automatic checks are on, none runs, and the first delay or the
    /// interval since the last one has passed.
    pub fn auto_due(&self, auto_check: bool, since_start: Duration) -> bool {
        if !auto_check || self.in_flight {
            return false;
        }
        match self.last_started {
            None => since_start >= FIRST_CHECK_DELAY,
            Some(last) => since_start.saturating_sub(last) >= CHECK_INTERVAL,
        }
    }

    /// How long to sleep before asking again: until the next automatic check is
    /// due, at most [`MAX_TICK`] so a change of the setting is noticed.
    /// Zero when one is due now.
    pub fn wait_until_due(&self, auto_check: bool, since_start: Duration) -> Duration {
        if !auto_check || self.in_flight {
            return MAX_TICK;
        }
        let due = match self.last_started {
            None => FIRST_CHECK_DELAY,
            Some(last) => last + CHECK_INTERVAL,
        };
        due.saturating_sub(since_start).min(MAX_TICK)
    }

    /// Begin a check. Returns `false`, and changes nothing, when one is
    /// already running or an automatic check is not due. A manual check is
    /// always allowed otherwise, whatever the automatic setting.
    pub fn start(&mut self, trigger: Trigger, auto_check: bool, since_start: Duration) -> bool {
        if self.in_flight {
            return false;
        }
        if trigger == Trigger::Auto && !self.auto_due(auto_check, since_start) {
            return false;
        }
        self.in_flight = true;
        // Any check restarts the interval: no automatic check right after a
        // manual one.
        self.last_started = Some(since_start);
        true
    }

    /// The running check ended, whatever its result.
    pub fn finish(&mut self) {
        self.in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn first_automatic_check_after_start() {
        let mut scheduler = Scheduler::default();
        assert!(!scheduler.auto_due(true, secs(0)));
        assert!(!scheduler.auto_due(true, secs(29)));
        assert!(scheduler.auto_due(true, secs(30)));
        assert!(scheduler.start(Trigger::Auto, true, secs(30)));
        scheduler.finish();

        // Then every 6 hours while running.
        let six_hours = 6 * 60 * 60;
        assert!(!scheduler.auto_due(true, secs(30 + six_hours - 1)));
        assert!(scheduler.auto_due(true, secs(30 + six_hours)));
        assert!(scheduler.start(Trigger::Auto, true, secs(30 + six_hours)));
        scheduler.finish();
        assert!(!scheduler.auto_due(true, secs(30 + six_hours + 60)));
        assert!(scheduler.auto_due(true, secs(30 + 2 * six_hours)));
    }

    #[test]
    fn automatic_checks_off() {
        let mut scheduler = Scheduler::default();
        // Never due, at any time, and an automatic start is refused.
        for t in [0, 30, 3600, 10 * 24 * 3600] {
            assert!(!scheduler.auto_due(false, secs(t)));
            assert!(!scheduler.start(Trigger::Auto, false, secs(t)));
        }
        assert!(!scheduler.in_flight());
        // Until the user selects "Check now".
        assert!(scheduler.start(Trigger::Manual, false, secs(5)));
        assert!(scheduler.in_flight());
    }

    #[test]
    fn concurrent_check_is_ignored() {
        let mut scheduler = Scheduler::default();
        assert!(scheduler.start(Trigger::Manual, true, secs(100)));
        // "Check now" again, and the schedule, while one runs: nothing starts.
        assert!(!scheduler.start(Trigger::Manual, true, secs(101)));
        assert!(!scheduler.start(Trigger::Auto, true, secs(100 + CHECK_INTERVAL.as_secs())));
        assert!(!scheduler.auto_due(true, secs(100 + CHECK_INTERVAL.as_secs())));
        assert!(scheduler.in_flight());
        // The running check ends; the next one starts normally.
        scheduler.finish();
        assert!(scheduler.start(Trigger::Manual, true, secs(102)));
    }

    #[test]
    fn wait_until_due_counts_down_to_the_next_check() {
        let mut scheduler = Scheduler::default();
        assert_eq!(scheduler.wait_until_due(true, secs(0)), secs(30));
        assert_eq!(scheduler.wait_until_due(true, secs(10)), secs(20));
        assert_eq!(scheduler.wait_until_due(true, secs(30)), secs(0));
        // Off, or a check running: look again in a minute.
        assert_eq!(scheduler.wait_until_due(false, secs(0)), MAX_TICK);
        assert!(scheduler.start(Trigger::Auto, true, secs(30)));
        assert_eq!(scheduler.wait_until_due(true, secs(31)), MAX_TICK);
        scheduler.finish();
        // Far from the next one: capped, so turning the setting on is noticed.
        assert_eq!(scheduler.wait_until_due(true, secs(31)), MAX_TICK);
        let next = 30 + CHECK_INTERVAL.as_secs();
        assert_eq!(scheduler.wait_until_due(true, secs(next - 5)), secs(5));
        assert_eq!(scheduler.wait_until_due(true, secs(next)), secs(0));
    }

    #[test]
    fn a_manual_check_delays_the_next_automatic_one() {
        let mut scheduler = Scheduler::default();
        assert!(scheduler.start(Trigger::Manual, true, secs(10)));
        scheduler.finish();
        assert!(
            !scheduler.auto_due(true, secs(30)),
            "no automatic check 20 s after a manual one"
        );
        assert!(scheduler.auto_due(true, secs(10 + CHECK_INTERVAL.as_secs())));
    }
}
