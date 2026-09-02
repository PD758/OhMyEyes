pub mod config;
pub mod image_asset;
pub mod scheduler;

mod limited_read;

#[cfg(windows)]
pub mod windows;

pub mod app;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppCommand {
    OpenSettings,
    ShowNow,
    ToggleReminders,
    DisplayTopologyChanged,
    SessionNotificationsDelayed,
    SessionNotificationsReady,
    SessionNotificationsUnavailable(u32),
    SystemPause(SystemPauseReason),
    SystemResume(SystemPauseReason),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPauseReason {
    Power,
    Session,
}
