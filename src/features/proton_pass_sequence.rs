//! The plan of a Proton Pass paste sequence ("copy all"), without GTK: which
//! values are pasted and in what order, and when a step counts as pasted.
//! The clipboard side is `ui/proton_pass_paste.rs`; this file is unit-tested.
//!
//! A step counts as pasted when another app reads its text from the clipboard.
//! Apps often ask twice for one paste, so a read only starts a quiet period of
//! [`QUIET`]; the next step is offered once no read has come for that long.
//! The whole sequence ends after [`LIFETIME`].

use crate::i18n::gettext;
use spotty_proton_account::Zeroizing;
use std::time::{Duration, Instant};

/// Quiet time after the last read of a step before the next step is offered.
pub const QUIET: Duration = Duration::from_millis(300);
/// The whole sequence ends (and the clipboard is cleared) after this long.
pub const LIFETIME: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Username,
    Password,
    Code,
}

impl Step {
    pub fn label(self) -> String {
        match self {
            Step::Username => gettext("username"),
            Step::Password => gettext("password"),
            Step::Code => gettext("code"),
        }
    }
}

/// The values one login contributes. Each is wiped when it is dropped. The
/// one-time code is not here: it is read from Proton Pass when its step is pasted.
pub struct LoginValues {
    pub target: String,
    pub username: Option<Zeroizing<String>>,
    pub password: Option<Zeroizing<String>>,
    pub has_totp: bool,
}

/// The steps for a login in paste order. A value the login doesn't have is skipped.
pub fn steps_for(username: bool, password: bool, code: bool) -> Vec<Step> {
    let mut steps = Vec::with_capacity(3);
    if username {
        steps.push(Step::Username);
    }
    if password {
        steps.push(Step::Password);
    }
    if code {
        steps.push(Step::Code);
    }
    steps
}

/// What the sequence should do next, as answered by [`Plan::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// A read came too recently: look again after this long.
    Wait(Duration),
    /// The current step was pasted: offer the next one.
    Advance,
    /// Nothing has been read since the last check.
    Idle,
    /// The sequence has run for [`LIFETIME`].
    Expired,
}

#[derive(Debug, Clone)]
pub struct Plan {
    steps: Vec<Step>,
    index: usize,
    started: Instant,
    last_read: Option<Instant>,
}

impl Plan {
    pub fn new(steps: Vec<Step>, started: Instant) -> Plan {
        Plan { steps, index: 0, started, last_read: None }
    }

    /// Position of the step on offer (or of the step count, once finished).
    pub fn index(&self) -> usize {
        self.index
    }

    pub fn current(&self) -> Option<Step> {
        self.steps.get(self.index).copied()
    }

    pub fn finished(&self) -> bool {
        self.index >= self.steps.len()
    }

    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= LIFETIME
    }

    /// A text read of the current step has been served.
    pub fn note_read(&mut self, now: Instant) {
        if !self.finished() {
            self.last_read = Some(now);
        }
    }

    pub fn tick(&self, now: Instant) -> Tick {
        if self.expired(now) {
            return Tick::Expired;
        }
        let Some(read) = self.last_read else {
            return Tick::Idle;
        };
        let quiet = now.saturating_duration_since(read);
        if quiet < QUIET {
            Tick::Wait(QUIET - quiet)
        } else {
            Tick::Advance
        }
    }

    /// Move on to the next step once the current one has been pasted.
    pub fn advance(&mut self) {
        self.index += 1;
        self.last_read = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn proton_pass_sequence_skips_the_values_a_login_lacks() {
        assert_eq!(steps_for(true, true, true), vec![Step::Username, Step::Password, Step::Code]);
        assert_eq!(steps_for(true, true, false), vec![Step::Username, Step::Password]);
        assert_eq!(steps_for(false, true, false), vec![Step::Password]);
        assert!(steps_for(false, false, false).is_empty());
    }

    #[test]
    fn proton_pass_sequence_advances_after_a_quiet_read() {
        let base = Instant::now();
        let mut plan = Plan::new(steps_for(true, true, false), base);
        assert_eq!(plan.current(), Some(Step::Username));
        plan.note_read(base);
        assert_eq!(plan.tick(at(base, 100)), Tick::Wait(QUIET - Duration::from_millis(100)));
        assert_eq!(plan.tick(at(base, 300)), Tick::Advance);
        plan.advance();
        assert_eq!(plan.current(), Some(Step::Password));
        assert!(!plan.finished());
        plan.note_read(at(base, 400));
        plan.advance();
        assert!(plan.finished());
        assert_eq!(plan.current(), None);
    }

    #[test]
    fn proton_pass_sequence_waits_out_a_second_read_of_the_same_step() {
        // Some apps ask for the text twice in a row: the step must stay put.
        let base = Instant::now();
        let mut plan = Plan::new(steps_for(true, true, false), base);
        plan.note_read(base);
        plan.note_read(at(base, 200));
        assert_eq!(plan.tick(at(base, 300)), Tick::Wait(Duration::from_millis(200)));
        assert_eq!(plan.current(), Some(Step::Username));
        assert_eq!(plan.tick(at(base, 500)), Tick::Advance);
    }

    #[test]
    fn proton_pass_sequence_does_not_advance_without_a_paste() {
        // Format queries never reach the write path, so nothing is read.
        let base = Instant::now();
        let plan = Plan::new(steps_for(true, true, true), base);
        assert_eq!(plan.tick(at(base, 5_000)), Tick::Idle);
    }

    #[test]
    fn proton_pass_sequence_expires_after_its_lifetime() {
        let base = Instant::now();
        let mut plan = Plan::new(steps_for(true, false, true), base);
        plan.note_read(at(base, 59_900));
        assert_eq!(plan.tick(at(base, 59_999)), Tick::Wait(Duration::from_millis(201)));
        assert!(!plan.expired(at(base, 59_999)));
        assert_eq!(plan.tick(at(base, LIFETIME.as_millis() as u64)), Tick::Expired);
        assert!(plan.expired(at(base, LIFETIME.as_millis() as u64)));
    }

    #[test]
    fn proton_pass_sequence_ignores_reads_after_the_last_step() {
        let base = Instant::now();
        let mut plan = Plan::new(steps_for(false, true, false), base);
        plan.note_read(base);
        plan.advance();
        assert!(plan.finished());
        plan.note_read(at(base, 10));
        assert_eq!(plan.tick(at(base, 10_000)), Tick::Idle);
    }
}
