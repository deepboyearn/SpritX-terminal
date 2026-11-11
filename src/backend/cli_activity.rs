//! Presentation-only activity for arbitrary CLIs; never changes process status.
//!
//! An open interactive process is not necessarily doing a task. Output changes
//! and submitted input give short-lived activity hints. Silence is only idle,
//! never proof of successful completion (that still comes from shell/exit data).

use std::{
    hash::{DefaultHasher, Hash, Hasher},
    time::{Duration, Instant},
};

pub const QUIET_AFTER: Duration = Duration::from_secs(3);
const ECHO_GRACE: Duration = Duration::from_millis(250);

#[derive(Default)]
pub struct CliActivity {
    screen: Option<u64>,
    last_work: Option<Instant>,
    echo_until: Option<Instant>,
}

pub fn screen_fingerprint(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl CliActivity {
    pub fn command_started(&mut self) {
        self.last_work = None;
        self.echo_until = None;
    }

    pub fn input(&mut self, bytes: &[u8], foreground_running: bool, now: Instant) {
        if bytes.contains(&3) {
            // Ctrl+C is a request to interrupt, not new work.
            self.last_work = None;
        } else if foreground_running && bytes.iter().any(|b| matches!(b, b'\r' | b'\n')) {
            self.last_work = Some(now);
        }
        // Local echo, backspace and TUI input redraws should not look like work.
        self.echo_until = Some(now + ECHO_GRACE);
    }

    pub fn observe_screen(&mut self, fingerprint: u64, foreground_running: bool, now: Instant) {
        let previous = self.screen.replace(fingerprint);
        if !foreground_running {
            self.last_work = None;
            return;
        }
        // First paint/reconnect is a baseline, not fresh activity. A repaint,
        // cursor blink or ANSI-only heartbeat does not change the fingerprint.
        if previous.is_some_and(|old| old != fingerprint)
            && self.echo_until.is_none_or(|until| now >= until)
        {
            self.last_work = Some(now);
        }
    }

    pub fn active(&self, now: Instant) -> bool {
        self.last_work
            .is_some_and(|at| now.saturating_duration_since(at) < QUIET_AFTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_open_cli_stays_idle_until_output_changes() {
        let now = Instant::now();
        let mut activity = CliActivity::default();
        activity.observe_screen(screen_fingerprint("unknown-cli ready >"), true, now);
        assert!(!activity.active(now));
        activity.observe_screen(screen_fingerprint("processing task"), true, now);
        assert!(activity.active(now));
        assert!(!activity.active(now + QUIET_AFTER));
        // Silence does not manufacture an exit code or completion event.
        activity.observe_screen(
            screen_fingerprint("processing task"),
            true,
            now + QUIET_AFTER,
        );
        assert!(!activity.active(now + QUIET_AFTER));
        activity.observe_screen(screen_fingerprint("new output"), true, now + QUIET_AFTER);
        assert!(activity.active(now + QUIET_AFTER));
    }

    #[test]
    fn unchanged_redraws_cannot_keep_an_idle_cli_animated() {
        let now = Instant::now();
        let mut activity = CliActivity::default();
        activity.observe_screen(1, true, now);
        activity.observe_screen(2, true, now);
        for second in 1..10 {
            activity.observe_screen(2, true, now + Duration::from_secs(second));
        }
        assert!(!activity.active(now + Duration::from_secs(9)));
    }

    #[test]
    fn typing_echo_does_not_animate_but_submitting_a_cli_task_does() {
        let now = Instant::now();
        let mut activity = CliActivity::default();
        activity.observe_screen(1, true, now);
        activity.input(b"hello", true, now);
        activity.observe_screen(2, true, now + Duration::from_millis(20));
        assert!(!activity.active(now));
        activity.input(b"\r", true, now);
        assert!(activity.active(now));
        activity.input(&[3], true, now);
        assert!(!activity.active(now));
        activity.input(b"\r", false, now);
        assert!(!activity.active(now));
    }

    #[test]
    fn completion_and_a_new_process_clear_previous_work() {
        let now = Instant::now();
        let mut activity = CliActivity::default();
        activity.observe_screen(1, true, now);
        activity.observe_screen(2, true, now);
        activity.observe_screen(3, false, now);
        assert!(!activity.active(now));
        activity.input(b"\r", true, now);
        activity.command_started();
        assert!(!activity.active(now));
    }
}
