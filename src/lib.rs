#![cfg_attr(not(windows), forbid(unsafe_code))]

pub mod config;
pub mod image_asset;
pub mod scheduler;

mod limited_read;

#[cfg(any(windows, target_os = "linux"))]
pub mod ipc;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
pub mod linux_wayland;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "linux")]
pub mod linux_daemon;

pub mod app;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppCommand {
    OpenSettings,
    RunInBackground,
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
