use std::time::Duration;

use crate::config::ResumePolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerAction {
    None,
    Show,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Disabled,
    Waiting {
        due_at: Duration,
    },
    Showing {
        hide_at: Duration,
        resume_enabled: bool,
    },
    OnBreak {
        remaining: Duration,
        was_showing: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ReminderScheduler {
    interval: Duration,
    duration: Duration,
    state: State,
}

impl ReminderScheduler {
    pub fn new(now: Duration, interval: Duration, duration: Duration, enabled: bool) -> Self {
        let state = if enabled {
            State::Waiting {
                due_at: now + interval,
            }
        } else {
            State::Disabled
        };
        Self {
            interval,
            duration,
            state,
        }
    }

    pub fn tick(&mut self, now: Duration) -> SchedulerAction {
        match self.state {
            State::Waiting { due_at } if now >= due_at => {
                self.state = State::Showing {
                    hide_at: now + self.duration,
                    resume_enabled: true,
                };
                SchedulerAction::Show
            }
            State::Showing {
                hide_at,
                resume_enabled,
            } if now >= hide_at => {
                self.state = if resume_enabled {
                    State::Waiting {
                        due_at: now + self.interval,
                    }
                } else {
                    State::Disabled
                };
                SchedulerAction::Hide
            }
            _ => SchedulerAction::None,
        }
    }

    pub fn reset(
        &mut self,
        now: Duration,
        interval: Duration,
        duration: Duration,
        enabled: bool,
    ) -> SchedulerAction {
        let was_showing = self.is_showing();
        self.interval = interval;
        self.duration = duration;
        self.state = if enabled {
            State::Waiting {
                due_at: now + interval,
            }
        } else {
            State::Disabled
        };
        if was_showing {
            SchedulerAction::Hide
        } else {
            SchedulerAction::None
        }
    }

    pub fn show_now(&mut self, now: Duration) -> SchedulerAction {
        if matches!(self.state, State::OnBreak { .. }) {
            return SchedulerAction::None;
        }
        let resume_enabled = match self.state {
            State::Disabled => false,
            State::Showing { resume_enabled, .. } => resume_enabled,
            State::Waiting { .. } => true,
            State::OnBreak { .. } => return SchedulerAction::None,
        };
        self.state = State::Showing {
            hide_at: now + self.duration,
            resume_enabled,
        };
        SchedulerAction::Show
    }

    pub fn begin_break(&mut self, now: Duration) -> SchedulerAction {
        let (remaining, was_showing) = match self.state {
            State::Waiting { due_at } => (due_at.saturating_sub(now), false),
            State::Showing {
                resume_enabled: true,
                ..
            } => (self.interval, true),
            State::Showing {
                resume_enabled: false,
                ..
            } => {
                self.state = State::Disabled;
                return SchedulerAction::Hide;
            }
            State::Disabled | State::OnBreak { .. } => return SchedulerAction::None,
        };
        self.state = State::OnBreak {
            remaining,
            was_showing,
        };
        if was_showing {
            SchedulerAction::Hide
        } else {
            SchedulerAction::None
        }
    }

    pub fn end_break(&mut self, now: Duration, policy: ResumePolicy) {
        let State::OnBreak { remaining, .. } = self.state else {
            return;
        };
        let delay = match policy {
            ResumePolicy::Reset => self.interval,
            ResumePolicy::Continue => remaining,
        };
        self.state = State::Waiting {
            due_at: now + delay,
        };
    }

    pub fn is_showing(&self) -> bool {
        matches!(self.state, State::Showing { .. })
    }

    pub fn next_wake_in(&self, now: Duration) -> Option<Duration> {
        match self.state {
            State::Waiting { due_at } => Some(due_at.saturating_sub(now)),
            State::Showing { hide_at, .. } => Some(hide_at.saturating_sub(now)),
            State::Disabled | State::OnBreak { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);
    const TWENTY_SECONDS: Duration = Duration::from_secs(20);

    fn scheduler() -> ReminderScheduler {
        ReminderScheduler::new(Duration::ZERO, 20 * MINUTE, TWENTY_SECONDS, true)
    }

    #[test]
    fn interval_starts_after_overlay_hides() {
        let mut scheduler = scheduler();
        assert_eq!(scheduler.tick(20 * MINUTE), SchedulerAction::Show);
        assert_eq!(
            scheduler.tick(20 * MINUTE + TWENTY_SECONDS),
            SchedulerAction::Hide
        );
        assert_eq!(scheduler.tick(40 * MINUTE), SchedulerAction::None);
        assert_eq!(
            scheduler.tick(40 * MINUTE + TWENTY_SECONDS),
            SchedulerAction::Show
        );
    }

    #[test]
    fn reset_policy_starts_a_full_interval_after_break() {
        let mut scheduler = scheduler();
        scheduler.begin_break(10 * MINUTE);
        scheduler.end_break(100 * MINUTE, ResumePolicy::Reset);
        assert_eq!(scheduler.tick(119 * MINUTE), SchedulerAction::None);
        assert_eq!(scheduler.tick(120 * MINUTE), SchedulerAction::Show);
    }

    #[test]
    fn continue_policy_preserves_remaining_time() {
        let mut scheduler = scheduler();
        scheduler.begin_break(7 * MINUTE);
        scheduler.end_break(100 * MINUTE, ResumePolicy::Continue);
        assert_eq!(scheduler.tick(112 * MINUTE), SchedulerAction::None);
        assert_eq!(scheduler.tick(113 * MINUTE), SchedulerAction::Show);
    }

    #[test]
    fn disabling_hides_an_active_overlay() {
        let mut scheduler = scheduler();
        scheduler.show_now(Duration::ZERO);
        assert_eq!(
            scheduler.reset(Duration::from_secs(1), 20 * MINUTE, TWENTY_SECONDS, false),
            SchedulerAction::Hide
        );
        assert_eq!(scheduler.next_wake_in(Duration::ZERO), None);
    }

    #[test]
    fn manual_reminder_works_while_scheduling_is_disabled() {
        let mut scheduler =
            ReminderScheduler::new(Duration::ZERO, 20 * MINUTE, TWENTY_SECONDS, false);
        assert_eq!(scheduler.show_now(Duration::ZERO), SchedulerAction::Show);
        assert!(scheduler.is_showing());
        assert_eq!(scheduler.tick(TWENTY_SECONDS), SchedulerAction::Hide);
        assert_eq!(scheduler.next_wake_in(TWENTY_SECONDS), None);
    }
}
