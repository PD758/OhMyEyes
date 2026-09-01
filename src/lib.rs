pub mod config;
pub mod image_asset;
pub mod scheduler;

#[cfg(windows)]
pub mod windows;

pub mod app;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppCommand {
    OpenSettings,
    ShowNow,
    ToggleReminders,
    DisplayTopologyChanged,
    SystemPause(SystemPauseReason),
    SystemResume(SystemPauseReason),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemPauseReason {
    Power,
    Session,
}
