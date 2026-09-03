use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use crate::{
    AppCommand,
    config::{ConfigStore, Settings},
    image_asset::{DecodedImage, load_default, load_file},
    ipc,
    linux_wayland::{NativeDisplay, OverlayController},
    scheduler::{ReminderScheduler, SchedulerAction},
};

pub const BACKGROUND_TAKEOVER_ARGUMENT: &str = "--background-takeover";
pub const FOREGROUND_TAKEOVER_ARGUMENT: &str = "--foreground-takeover";
const ERROR_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub fn run(show_now: bool) -> Result<(), String> {
    let (commands_tx, commands_rx) = mpsc::channel();
    let _ipc_server =
        ipc::start_ipc_server(commands_tx.clone()).map_err(|error| error.to_string())?;
    let (overlay, displays) = OverlayController::create(commands_tx, None)?;
    let config_store = ConfigStore::for_current_user()
        .unwrap_or_else(|_| ConfigStore::new(PathBuf::from("OhMyEyes-config.json")));
    let mut settings = match config_store.load() {
        Ok(loaded) => loaded.settings,
        Err(error) => {
            tracing::warn!(%error, "settings could not be loaded; using defaults");
            Settings::default()
        }
    };
    let executable_dir = executable_directory();
    let image = load_configured_image(&mut settings, &config_store, &executable_dir)?;
    let selected_display = selected_display(&settings, &displays);
    let started_at = Instant::now();
    let mut daemon = Daemon {
        started_at,
        scheduler: ReminderScheduler::new(
            Duration::ZERO,
            settings.interval(),
            settings.overlay_duration(),
            settings.reminders_enabled,
        ),
        settings,
        config_store,
        image,
        displays,
        selected_display,
        animation_started_at: Duration::ZERO,
        animation_frame_index: 0,
        overlay,
        commands_rx,
    };
    if show_now {
        let action = daemon.scheduler.show_now(daemon.now());
        daemon.apply_action(action)?;
    }
    daemon.event_loop()
}

pub fn spawn_background_takeover() -> Result<(), String> {
    spawn_takeover(BACKGROUND_TAKEOVER_ARGUMENT)
}

fn spawn_foreground_takeover() -> Result<(), String> {
    spawn_takeover(FOREGROUND_TAKEOVER_ARGUMENT)
}

fn spawn_takeover(argument: &str) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    Command::new(executable)
        .arg(argument)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

struct Daemon {
    started_at: Instant,
    scheduler: ReminderScheduler,
    settings: Settings,
    config_store: ConfigStore,
    image: DecodedImage,
    displays: Vec<NativeDisplay>,
    selected_display: usize,
    animation_started_at: Duration,
    animation_frame_index: usize,
    overlay: OverlayController,
    commands_rx: Receiver<AppCommand>,
}

impl Daemon {
    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn event_loop(&mut self) -> Result<(), String> {
        loop {
            let action = self.scheduler.tick(self.now());
            self.apply_action(action)?;
            let next_animation = self.update_animation()?;
            if let Some(error) = self.overlay.take_error() {
                tracing::warn!(%error, "Wayland overlay command failed");
            }
            let wait = match (self.scheduler.next_wake_in(self.now()), next_animation) {
                (Some(schedule), Some(animation)) => schedule.min(animation),
                (Some(schedule), None) => schedule,
                (None, Some(animation)) => animation,
                (None, None) => ERROR_POLL_INTERVAL,
            }
            .min(ERROR_POLL_INTERVAL);
            match self.commands_rx.recv_timeout(wait) {
                Ok(command) if self.process_command(command)? => return Ok(()),
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("daemon command channel disconnected".to_owned());
                }
            }
        }
    }

    fn process_command(&mut self, command: AppCommand) -> Result<bool, String> {
        match command {
            AppCommand::OpenSettings => {
                spawn_foreground_takeover()?;
                return Ok(true);
            }
            AppCommand::RunInBackground => {}
            AppCommand::ShowNow => {
                let action = self.scheduler.show_now(self.now());
                self.apply_action(action)?;
            }
            AppCommand::ToggleReminders => {
                self.settings.reminders_enabled = !self.settings.reminders_enabled;
                let action = self.scheduler.reset(
                    self.now(),
                    self.settings.interval(),
                    self.settings.overlay_duration(),
                    self.settings.reminders_enabled,
                );
                self.apply_action(action)?;
                self.config_store
                    .save(&self.settings)
                    .map_err(|error| error.to_string())?;
            }
            AppCommand::DisplayTopologyChanged => {
                if let Err(error) = self.refresh_displays() {
                    tracing::warn!(%error, "Wayland outputs could not be refreshed");
                }
            }
            AppCommand::Quit => return Ok(true),
            AppCommand::SessionNotificationsDelayed
            | AppCommand::SessionNotificationsReady
            | AppCommand::SessionNotificationsUnavailable(_)
            | AppCommand::SystemPause(_)
            | AppCommand::SystemResume(_) => {}
        }
        Ok(false)
    }

    fn apply_action(&mut self, action: SchedulerAction) -> Result<(), String> {
        match action {
            SchedulerAction::Show => {
                self.animation_started_at = self.now();
                self.animation_frame_index = 0;
                self.show_overlay()?;
            }
            SchedulerAction::Hide => self.overlay.hide()?,
            SchedulerAction::None => {}
        }
        Ok(())
    }

    fn update_animation(&mut self) -> Result<Option<Duration>, String> {
        if !self.scheduler.is_showing() || !self.image.is_animated() {
            return Ok(None);
        }
        let elapsed = self.now().saturating_sub(self.animation_started_at);
        let (index, next_frame_in) = self.image.frame_at(elapsed);
        if index != self.animation_frame_index {
            self.animation_frame_index = index;
            self.show_overlay()?;
        }
        Ok(Some(next_frame_in))
    }

    fn show_overlay(&self) -> Result<(), String> {
        let display = self
            .displays
            .get(self.selected_display)
            .ok_or_else(|| "Wayland compositor reported no usable output".to_owned())?;
        self.overlay.show(
            &display.id,
            &self.image.frame_or_first(self.animation_frame_index).rgba,
            self.image.size,
            self.settings.width_percent,
            self.settings.opacity_percent,
            [self.settings.position.x, self.settings.position.y],
        )
    }

    fn refresh_displays(&mut self) -> Result<(), String> {
        let selected_id = self
            .displays
            .get(self.selected_display)
            .map(|display| display.id.clone());
        let displays = self.overlay.displays()?;
        if displays.is_empty() {
            return Err("Wayland compositor reported no outputs".to_owned());
        }
        self.selected_display = selected_id
            .as_ref()
            .and_then(|id| displays.iter().position(|display| &display.id == id))
            .unwrap_or(0);
        if selected_id.is_some()
            && selected_id
                .as_ref()
                .is_none_or(|id| displays.iter().all(|display| &display.id != id))
        {
            self.settings.display_id = displays.first().map(|display| display.id.clone());
            self.config_store
                .save(&self.settings)
                .map_err(|error| error.to_string())?;
        }
        self.displays = displays;
        if self.scheduler.is_showing() {
            self.show_overlay()?;
        }
        Ok(())
    }
}

fn executable_directory() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn selected_display(settings: &Settings, displays: &[NativeDisplay]) -> usize {
    settings
        .display_id
        .as_ref()
        .and_then(|id| displays.iter().position(|display| &display.id == id))
        .unwrap_or(0)
}

fn load_configured_image(
    settings: &mut Settings,
    config_store: &ConfigStore,
    executable_dir: &Path,
) -> Result<DecodedImage, String> {
    let Some(path) = settings.resolve_image_path(executable_dir) else {
        return load_default().map_err(|error| error.to_string());
    };
    match load_file(&path) {
        Ok(image) => Ok(image),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "configured image could not be loaded");
            settings.image_path = None;
            config_store
                .save(settings)
                .map_err(|error| error.to_string())?;
            load_default().map_err(|error| error.to_string())
        }
    }
}
