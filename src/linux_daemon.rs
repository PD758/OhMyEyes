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

fn next_wait(schedule: Option<Duration>, animation: Option<Duration>) -> Duration {
    match (schedule, animation) {
        (Some(schedule), Some(animation)) => schedule.min(animation),
        (Some(schedule), None) => schedule,
        (None, Some(animation)) => animation,
        (None, None) => ERROR_POLL_INTERVAL,
    }
    .min(ERROR_POLL_INTERVAL)
}

trait OverlayBackend {
    #[allow(clippy::too_many_arguments)]
    fn show(
        &self,
        display_id: &str,
        rgba: &[u8],
        image_size: [u32; 2],
        width_percent: u8,
        opacity_percent: u8,
        position: [f32; 2],
    ) -> Result<(), String>;
    fn hide(&self) -> Result<(), String>;
    fn displays(&self) -> Result<Vec<NativeDisplay>, String>;
    fn take_error(&self) -> Option<String>;
}

impl OverlayBackend for OverlayController {
    fn show(
        &self,
        display_id: &str,
        rgba: &[u8],
        image_size: [u32; 2],
        width_percent: u8,
        opacity_percent: u8,
        position: [f32; 2],
    ) -> Result<(), String> {
        OverlayController::show(
            self,
            display_id,
            rgba,
            image_size,
            width_percent,
            opacity_percent,
            position,
        )
    }

    fn hide(&self) -> Result<(), String> {
        OverlayController::hide(self)
    }

    fn displays(&self) -> Result<Vec<NativeDisplay>, String> {
        OverlayController::displays(self)
    }

    fn take_error(&self) -> Option<String> {
        OverlayController::take_error(self)
    }
}

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

struct Daemon<O: OverlayBackend = OverlayController> {
    started_at: Instant,
    scheduler: ReminderScheduler,
    settings: Settings,
    config_store: ConfigStore,
    image: DecodedImage,
    displays: Vec<NativeDisplay>,
    selected_display: usize,
    animation_started_at: Duration,
    animation_frame_index: usize,
    overlay: O,
    commands_rx: Receiver<AppCommand>,
}

impl<O: OverlayBackend> Daemon<O> {
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
            let wait = next_wait(self.scheduler.next_wake_in(self.now()), next_animation);
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

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        sync::mpsc::Sender,
    };

    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    struct ShowCall {
        display_id: String,
        image_size: [u32; 2],
        width_percent: u8,
        opacity_percent: u8,
        position: [f32; 2],
    }

    struct FakeOverlay {
        show_calls: RefCell<Vec<ShowCall>>,
        hide_count: Cell<usize>,
        displays: RefCell<Result<Vec<NativeDisplay>, String>>,
        error: RefCell<Option<String>>,
    }

    impl FakeOverlay {
        fn new(displays: Vec<NativeDisplay>) -> Self {
            Self {
                show_calls: RefCell::new(Vec::new()),
                hide_count: Cell::new(0),
                displays: RefCell::new(Ok(displays)),
                error: RefCell::new(None),
            }
        }
    }

    impl OverlayBackend for FakeOverlay {
        fn show(
            &self,
            display_id: &str,
            _rgba: &[u8],
            image_size: [u32; 2],
            width_percent: u8,
            opacity_percent: u8,
            position: [f32; 2],
        ) -> Result<(), String> {
            self.show_calls.borrow_mut().push(ShowCall {
                display_id: display_id.to_owned(),
                image_size,
                width_percent,
                opacity_percent,
                position,
            });
            Ok(())
        }

        fn hide(&self) -> Result<(), String> {
            self.hide_count.set(self.hide_count.get() + 1);
            Ok(())
        }

        fn displays(&self) -> Result<Vec<NativeDisplay>, String> {
            self.displays.borrow().clone()
        }

        fn take_error(&self) -> Option<String> {
            self.error.borrow_mut().take()
        }
    }

    fn display(id: &str) -> NativeDisplay {
        NativeDisplay {
            id: id.to_owned(),
            label: id.to_owned(),
        }
    }

    fn daemon(
        settings: Settings,
        displays: Vec<NativeDisplay>,
        config_store: ConfigStore,
        commands_rx: Receiver<AppCommand>,
    ) -> Daemon<FakeOverlay> {
        let scheduler = ReminderScheduler::new(
            Duration::ZERO,
            settings.interval(),
            settings.overlay_duration(),
            settings.reminders_enabled,
        );
        Daemon {
            started_at: Instant::now(),
            scheduler,
            settings,
            config_store,
            image: load_default().expect("default image should load"),
            selected_display: 0,
            overlay: FakeOverlay::new(displays.clone()),
            displays,
            animation_started_at: Duration::ZERO,
            animation_frame_index: 0,
            commands_rx,
        }
    }

    fn command_channel() -> (Sender<AppCommand>, Receiver<AppCommand>) {
        mpsc::channel()
    }

    #[test]
    fn display_selection_uses_configured_id_or_first_output() {
        let displays = vec![display("left"), display("right")];
        let configured = Settings {
            display_id: Some("right".to_owned()),
            ..Settings::default()
        };
        let missing = Settings {
            display_id: Some("missing".to_owned()),
            ..Settings::default()
        };

        assert_eq!(selected_display(&configured, &displays), 1);
        assert_eq!(selected_display(&missing, &displays), 0);
        assert_eq!(selected_display(&configured, &[]), 0);
    }

    #[test]
    fn configured_image_falls_back_and_persists_the_repair() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let mut settings = Settings {
            image_path: Some(PathBuf::from("missing.png")),
            ..Settings::default()
        };

        let image = load_configured_image(&mut settings, &store, directory.path())
            .expect("default image should be used");

        assert_eq!(
            image.size,
            load_default().expect("default image should load").size
        );
        assert_eq!(settings.image_path, None);
        assert_eq!(
            store
                .load()
                .expect("repaired settings should load")
                .settings
                .image_path,
            None
        );
    }

    #[test]
    fn configured_image_loads_default_and_valid_file() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let mut defaults = Settings::default();
        let default_image = load_configured_image(&mut defaults, &store, directory.path())
            .expect("default image should load");
        let image_path = directory.path().join("eye.png");
        std::fs::write(&image_path, crate::image_asset::DEFAULT_EYE_BYTES)
            .expect("test image should be written");
        let mut configured = Settings {
            image_path: Some(image_path),
            ..Settings::default()
        };
        let configured_image = load_configured_image(&mut configured, &store, directory.path())
            .expect("configured image should load");

        assert_eq!(configured_image.size, default_image.size);
    }

    #[test]
    fn daemon_processes_overlay_and_scheduler_commands() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let (_sender, receiver) = command_channel();
        let mut daemon = daemon(
            Settings::default(),
            vec![display("primary")],
            store,
            receiver,
        );

        assert!(
            !daemon
                .process_command(AppCommand::RunInBackground)
                .expect("background command should succeed")
        );
        assert!(
            !daemon
                .process_command(AppCommand::SessionNotificationsReady)
                .expect("irrelevant native command should be ignored")
        );
        assert!(
            !daemon
                .process_command(AppCommand::ShowNow)
                .expect("show command should succeed")
        );
        assert_eq!(daemon.overlay.show_calls.borrow().len(), 1);
        assert!(
            !daemon
                .process_command(AppCommand::ToggleReminders)
                .expect("toggle command should succeed")
        );
        assert!(!daemon.settings.reminders_enabled);
        assert_eq!(daemon.overlay.hide_count.get(), 1);
        *daemon.overlay.displays.borrow_mut() = Err("enumeration failed".to_owned());
        assert!(
            !daemon
                .process_command(AppCommand::DisplayTopologyChanged)
                .expect("display errors should be contained")
        );
        assert!(
            daemon
                .process_command(AppCommand::Quit)
                .expect("quit command should succeed")
        );
    }

    #[test]
    fn display_refresh_preserves_or_repairs_selection() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let settings = Settings {
            display_id: Some("right".to_owned()),
            ..Settings::default()
        };
        let (_sender, receiver) = command_channel();
        let mut daemon = daemon(
            settings,
            vec![display("left"), display("right")],
            store,
            receiver,
        );
        daemon.selected_display = 1;
        daemon
            .process_command(AppCommand::ShowNow)
            .expect("overlay should be shown");
        *daemon.overlay.displays.borrow_mut() = Ok(vec![display("right"), display("left")]);
        daemon.refresh_displays().expect("outputs should refresh");
        assert_eq!(daemon.selected_display, 0);
        assert_eq!(daemon.settings.display_id.as_deref(), Some("right"));
        assert_eq!(daemon.overlay.show_calls.borrow().len(), 2);

        *daemon.overlay.displays.borrow_mut() = Ok(vec![display("replacement")]);
        daemon
            .refresh_displays()
            .expect("missing output should be repaired");
        assert_eq!(daemon.settings.display_id.as_deref(), Some("replacement"));

        *daemon.overlay.displays.borrow_mut() = Ok(Vec::new());
        assert_eq!(
            daemon
                .refresh_displays()
                .expect_err("empty outputs should fail"),
            "Wayland compositor reported no outputs"
        );
    }

    #[test]
    fn event_loop_handles_pending_error_and_quit() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let (sender, receiver) = command_channel();
        sender
            .send(AppCommand::Quit)
            .expect("quit should be queued");
        let mut daemon = daemon(
            Settings::default(),
            vec![display("primary")],
            store,
            receiver,
        );
        *daemon.overlay.error.borrow_mut() = Some("test error".to_owned());

        daemon
            .event_loop()
            .expect("queued quit should stop the loop");
        assert_eq!(
            daemon.update_animation().expect("static image is valid"),
            None
        );
        assert!(executable_directory().is_absolute());
    }

    #[test]
    fn event_loop_reports_disconnected_command_channel() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let (sender, receiver) = command_channel();
        drop(sender);
        let settings = Settings {
            reminders_enabled: false,
            ..Settings::default()
        };
        let mut daemon = daemon(settings, vec![display("primary")], store, receiver);

        assert_eq!(
            daemon
                .event_loop()
                .expect_err("disconnected channel should stop the loop"),
            "daemon command channel disconnected"
        );
    }

    #[test]
    fn daemon_wait_uses_the_earliest_bounded_deadline() {
        assert_eq!(
            next_wait(
                Some(Duration::from_millis(800)),
                Some(Duration::from_millis(20))
            ),
            Duration::from_millis(20)
        );
        assert_eq!(
            next_wait(Some(Duration::from_millis(30)), None),
            Duration::from_millis(30)
        );
        assert_eq!(
            next_wait(None, Some(Duration::from_millis(40))),
            Duration::from_millis(40)
        );
        assert_eq!(next_wait(None, None), ERROR_POLL_INTERVAL);
        assert_eq!(
            next_wait(Some(Duration::from_secs(20)), None),
            ERROR_POLL_INTERVAL
        );
    }

    #[test]
    fn showing_without_a_display_is_reported() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let (_sender, receiver) = command_channel();
        let mut daemon = daemon(Settings::default(), Vec::new(), store, receiver);

        assert_eq!(
            daemon
                .process_command(AppCommand::ShowNow)
                .expect_err("showing without outputs should fail"),
            "Wayland compositor reported no usable output"
        );
    }
}
