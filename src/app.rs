use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

use eframe::{CreationContext, Frame, egui};

use crate::{
    AppCommand, SystemPauseReason,
    config::{ConfigStore, Settings},
    image_asset::{DecodedImage, ImageAssetError, load_default, load_file},
    scheduler::{ReminderScheduler, SchedulerAction},
};

#[cfg(windows)]
use crate::config::ResumePolicy;

#[cfg(not(any(windows, target_os = "linux")))]
fn overlay_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("ohmyeyes-overlay")
}

const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

pub struct AppStartup {
    commands_tx: Sender<AppCommand>,
    commands_rx: Receiver<AppCommand>,
    #[cfg(any(windows, target_os = "linux"))]
    ipc_server: Result<crate::ipc::IpcServer, String>,
}

impl AppStartup {
    pub fn initialize() -> Self {
        let (commands_tx, commands_rx) = mpsc::channel();
        #[cfg(any(windows, target_os = "linux"))]
        let ipc_server =
            crate::ipc::start_ipc_server(commands_tx.clone()).map_err(|error| error.to_string());
        Self {
            commands_tx,
            commands_rx,
            #[cfg(any(windows, target_os = "linux"))]
            ipc_server,
        }
    }
}

#[derive(Debug, Clone)]
struct DisplayInfo {
    id: String,
    #[cfg(windows)]
    legacy_id: String,
    label: String,
    #[cfg(not(any(windows, target_os = "linux")))]
    index: usize,
    #[cfg(windows)]
    left: i32,
    #[cfg(windows)]
    top: i32,
    #[cfg(windows)]
    width: u32,
    #[cfg(windows)]
    height: u32,
}

pub struct OhMyEyesApp {
    started_at: Instant,
    settings: Settings,
    config_store: ConfigStore,
    scheduler: ReminderScheduler,
    image: DecodedImage,
    texture: egui::TextureHandle,
    animation_started_at: Duration,
    animation_frame_index: usize,
    executable_dir: PathBuf,
    displays: Vec<DisplayInfo>,
    selected_display: usize,
    settings_open: bool,
    settings_size_initialized: bool,
    settings_save_due_at: Option<Duration>,
    root_visible: bool,
    #[cfg(not(any(windows, target_os = "linux")))]
    root_controller_mode: bool,
    quitting: bool,
    system_pause_mask: u8,
    session_notification_status: Option<String>,
    status: Option<String>,
    commands_rx: Receiver<AppCommand>,
    #[cfg(windows)]
    commands_tx: Sender<AppCommand>,
    #[cfg(windows)]
    tray: Option<crate::windows::TrayController>,
    #[cfg(windows)]
    overlay: Option<crate::windows::OverlayController>,
    #[cfg(target_os = "linux")]
    overlay: Option<crate::linux_wayland::OverlayController>,
}

impl OhMyEyesApp {
    pub fn new(
        cc: &CreationContext<'_>,
        background: bool,
        show_now: bool,
        startup: AppStartup,
    ) -> Result<Self, ImageAssetError> {
        let AppStartup {
            commands_tx,
            commands_rx,
            #[cfg(any(windows, target_os = "linux"))]
            ipc_server,
        } = startup;
        configure_style(&cc.egui_ctx);
        let config_store = ConfigStore::for_current_user()
            .unwrap_or_else(|_| ConfigStore::new(PathBuf::from("OhMyEyes-config.json")));
        let loaded = config_store.load();
        let (mut settings, mut status) = match loaded {
            Ok(loaded) => {
                let status = (!loaded.warnings.is_empty()).then(|| loaded.warnings.join("; "));
                (loaded.settings, status)
            }
            Err(error) => (
                Settings::default(),
                Some(format!("Could not load settings: {error}")),
            ),
        };
        let executable_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let mut image_selection_reset = false;
        let image = match settings.resolve_image_path(&executable_dir) {
            Some(path) => match load_file(&path) {
                Ok(image) => image,
                Err(error) => {
                    append_status(
                        &mut status,
                        format!(
                            "Could not load {}: {error}; using bundled eye",
                            path.display()
                        ),
                    );
                    settings.image_path = None;
                    image_selection_reset = true;
                    load_default()?
                }
            },
            None => load_default()?,
        };
        if image_selection_reset && let Err(error) = config_store.save(&settings) {
            append_status(
                &mut status,
                format!("Could not clear invalid image setting: {error}"),
            );
        }
        let texture = cc.egui_ctx.load_texture(
            "reminder-eye",
            image.color_image.clone(),
            egui::TextureOptions::LINEAR,
        );
        #[cfg(windows)]
        let (displays, display_enumeration_failed) = match crate::windows::enumerate_displays() {
            Ok(displays) => (displays.into_iter().map(DisplayInfo::from).collect(), false),
            Err(error) => {
                append_status(
                    &mut status,
                    format!("Could not enumerate displays: {error}"),
                );
                (Vec::new(), true)
            }
        };
        #[cfg(target_os = "linux")]
        let (overlay, displays, overlay_failed) =
            match crate::linux_wayland::OverlayController::create(
                commands_tx.clone(),
                Some(cc.egui_ctx.clone()),
            ) {
                Ok((overlay, displays)) => (
                    Some(overlay),
                    displays.into_iter().map(DisplayInfo::from).collect(),
                    false,
                ),
                Err(error) => {
                    append_status(
                        &mut status,
                        format!("Could not create Wayland reminder overlay: {error}"),
                    );
                    (None, Vec::new(), true)
                }
            };
        #[cfg(not(any(windows, target_os = "linux")))]
        let displays = enumerate_displays(cc);
        #[cfg(windows)]
        let (selected_display, display_id_migrated) =
            settings.display_id.as_ref().map_or((0, false), |id| {
                displays
                    .iter()
                    .position(|display| &display.id == id)
                    .map(|index| (index, false))
                    .or_else(|| {
                        displays
                            .iter()
                            .position(|display| &display.legacy_id == id)
                            .map(|index| (index, true))
                    })
                    .unwrap_or((0, false))
            });
        #[cfg(not(windows))]
        let selected_display = settings
            .display_id
            .as_ref()
            .and_then(|id| displays.iter().position(|display| &display.id == id))
            .unwrap_or(0);
        #[cfg(windows)]
        if display_id_migrated && let Some(display) = displays.get(selected_display) {
            settings.display_id = Some(display.id.clone());
            if let Err(error) = config_store.save(&settings) {
                append_status(
                    &mut status,
                    format!("Could not migrate the selected display identifier: {error}"),
                );
            }
        }
        let started_at = Instant::now();
        let scheduler = ReminderScheduler::new(
            Duration::ZERO,
            settings.interval(),
            settings.overlay_duration(),
            settings.reminders_enabled,
        );
        #[cfg(any(windows, target_os = "linux"))]
        let ipc_failed = match ipc_server {
            Ok(server) => {
                server.attach_context(cc.egui_ctx.clone());
                false
            }
            Err(error) => {
                tracing::warn!(%error, "IPC server could not start");
                append_status(
                    &mut status,
                    format!("Single-instance activation is unavailable: {error}"),
                );
                true
            }
        };
        #[cfg(windows)]
        let mut session_notification_status = None;
        #[cfg(not(windows))]
        let session_notification_status = None;
        #[cfg(windows)]
        if let Err(error) =
            crate::windows::start_system_event_monitor(commands_tx.clone(), cc.egui_ctx.clone())
        {
            tracing::warn!(%error, "system event monitor could not start");
            session_notification_status = Some(format!(
                "Lock and unlock monitoring is unavailable: {error}"
            ));
        }
        #[cfg(windows)]
        let overlay = match crate::windows::OverlayController::create() {
            Ok(overlay) => Some(overlay),
            Err(error) => {
                status = Some(format!("Could not create reminder overlay: {error}"));
                None
            }
        };
        #[cfg(windows)]
        let overlay_failed = overlay.is_none();

        #[cfg(windows)]
        let settings_open =
            !background || ipc_failed || overlay_failed || display_enumeration_failed;
        #[cfg(target_os = "linux")]
        let settings_open = !background || ipc_failed || overlay_failed;
        #[cfg(not(any(windows, target_os = "linux")))]
        let settings_open = !background;
        let mut app = Self {
            started_at,
            settings,
            config_store,
            scheduler,
            image,
            texture,
            animation_started_at: Duration::ZERO,
            animation_frame_index: 0,
            executable_dir,
            displays,
            selected_display,
            settings_open,
            settings_size_initialized: false,
            settings_save_due_at: None,
            root_visible: !background,
            #[cfg(not(any(windows, target_os = "linux")))]
            root_controller_mode: false,
            quitting: false,
            system_pause_mask: 0,
            session_notification_status,
            status,
            commands_rx,
            #[cfg(windows)]
            commands_tx,
            #[cfg(windows)]
            tray: None,
            #[cfg(windows)]
            overlay,
            #[cfg(target_os = "linux")]
            overlay,
        };
        app.refresh_tray(&cc.egui_ctx);
        if show_now {
            let action = app.scheduler.show_now(app.now());
            app.apply_scheduler_action(&cc.egui_ctx, action);
        }
        Ok(app)
    }

    fn now(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn apply_scheduler_action(&mut self, ctx: &egui::Context, action: SchedulerAction) {
        match action {
            SchedulerAction::Show => {
                self.animation_started_at = self.now();
                self.set_animation_frame(ctx, 0);
                self.refresh_overlay(ctx);
            }
            SchedulerAction::Hide => {
                #[cfg(windows)]
                if let Some(overlay) = &self.overlay
                    && let Err(error) = overlay.hide()
                {
                    self.status = Some(format!("Could not hide reminder overlay: {error}"));
                }
                #[cfg(target_os = "linux")]
                if let Some(overlay) = &self.overlay
                    && let Err(error) = overlay.hide()
                {
                    self.status = Some(format!("Could not hide Wayland reminder overlay: {error}"));
                }
                #[cfg(not(any(windows, target_os = "linux")))]
                ctx.send_viewport_cmd_to(overlay_viewport_id(), egui::ViewportCommand::Close);
            }
            SchedulerAction::None => {}
        }
        if action != SchedulerAction::None {
            self.update_root_visibility(ctx);
        }
    }

    fn update_root_visibility(&mut self, ctx: &egui::Context) {
        #[cfg(any(windows, target_os = "linux"))]
        {
            let visible = self.settings_open;
            if visible != self.root_visible {
                self.root_visible = visible;
                send_root_command(ctx, egui::ViewportCommand::Visible(visible));
            }
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let overlay_visible = self.scheduler.is_showing();
            if self.settings_open && self.root_controller_mode {
                self.root_controller_mode = false;
                send_root_command(ctx, egui::ViewportCommand::Fullscreen(false));
                send_root_command(ctx, egui::ViewportCommand::Decorations(true));
                send_root_command(ctx, egui::ViewportCommand::MousePassthrough(false));
                send_root_command(
                    ctx,
                    egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal),
                );
                send_root_command(
                    ctx,
                    egui::ViewportCommand::InnerSize(egui::vec2(560.0, 720.0)),
                );
                send_root_command(
                    ctx,
                    egui::ViewportCommand::OuterPosition(egui::pos2(100.0, 100.0)),
                );
                send_root_command(ctx, egui::ViewportCommand::Focus);
            } else if !self.settings_open && overlay_visible && !self.root_controller_mode {
                self.root_controller_mode = true;
                send_root_command(ctx, egui::ViewportCommand::Fullscreen(false));
                send_root_command(ctx, egui::ViewportCommand::Decorations(false));
                send_root_command(ctx, egui::ViewportCommand::MousePassthrough(true));
                send_root_command(
                    ctx,
                    egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal),
                );
                send_root_command(ctx, egui::ViewportCommand::InnerSize(egui::vec2(1.0, 1.0)));
                send_root_command(
                    ctx,
                    egui::ViewportCommand::OuterPosition(egui::pos2(-32_000.0, -32_000.0)),
                );
            }

            let visible = overlay_visible || self.settings_open;
            if visible != self.root_visible {
                self.root_visible = visible;
                send_root_command(ctx, egui::ViewportCommand::Visible(visible));
            }
        }
    }

    fn initialize_settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open || self.settings_size_initialized {
            return;
        }
        self.settings_size_initialized = true;
        send_root_command(ctx, egui::ViewportCommand::Fullscreen(false));
        send_root_command(ctx, egui::ViewportCommand::Decorations(true));
        send_root_command(ctx, egui::ViewportCommand::MousePassthrough(false));
        send_root_command(
            ctx,
            egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal),
        );
        send_root_command(
            ctx,
            egui::ViewportCommand::InnerSize(egui::vec2(560.0, 720.0)),
        );
    }

    fn refresh_overlay(&mut self, ctx: &egui::Context) {
        if !self.scheduler.is_showing() {
            return;
        }
        #[cfg(windows)]
        if let (Some(overlay), Some(display)) =
            (&self.overlay, self.displays.get(self.selected_display))
            && let Err(error) = overlay.show(
                display.left,
                display.top,
                display.width,
                display.height,
                &self.image.frame_or_first(self.animation_frame_index).rgba,
                self.image.size,
                self.settings.width_percent,
                self.settings.opacity_percent,
                [self.settings.position.x, self.settings.position.y],
            )
        {
            self.status = Some(format!("Could not show reminder overlay: {error}"));
        }
        #[cfg(target_os = "linux")]
        if let (Some(overlay), Some(display)) =
            (&self.overlay, self.displays.get(self.selected_display))
            && let Err(error) = overlay.show(
                &display.id,
                &self.image.frame_or_first(self.animation_frame_index).rgba,
                self.image.size,
                self.settings.width_percent,
                self.settings.opacity_percent,
                [self.settings.position.x, self.settings.position.y],
            )
        {
            self.status = Some(format!("Could not show Wayland reminder overlay: {error}"));
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        ctx.request_repaint();
        #[cfg(any(windows, target_os = "linux"))]
        let _ = ctx;
    }

    fn set_animation_frame(&mut self, ctx: &egui::Context, index: usize) {
        if self.animation_frame_index == index {
            return;
        }
        self.animation_frame_index = index;
        #[cfg(any(windows, target_os = "linux"))]
        if !self.settings_open {
            return;
        }
        self.update_preview_texture(ctx);
    }

    fn update_preview_texture(&mut self, ctx: &egui::Context) {
        let frame = self.image.frame_or_first(self.animation_frame_index);
        self.texture.set(
            egui::ColorImage::from_rgba_unmultiplied(
                [self.image.size[0] as usize, self.image.size[1] as usize],
                &frame.rgba,
            ),
            egui::TextureOptions::LINEAR,
        );
        ctx.request_repaint();
    }

    fn update_animation(&mut self, ctx: &egui::Context) -> Option<Duration> {
        if !self.scheduler.is_showing() || !self.image.is_animated() {
            return None;
        }
        let elapsed = self.now().saturating_sub(self.animation_started_at);
        let (index, next_frame_in) = self.image.frame_at(elapsed);
        if index != self.animation_frame_index {
            self.set_animation_frame(ctx, index);
            self.refresh_overlay(ctx);
        }
        Some(next_frame_in)
    }

    fn reset_schedule(&mut self, ctx: &egui::Context) {
        let action = self.scheduler.reset(
            self.now(),
            self.settings.interval(),
            self.settings.overlay_duration(),
            self.settings.reminders_enabled,
        );
        self.apply_scheduler_action(ctx, action);
    }

    fn save_settings(&mut self) {
        self.settings_save_due_at = None;
        if let Some(display) = self.displays.get(self.selected_display) {
            self.settings.display_id = Some(display.id.clone());
        }
        match self.config_store.save(&self.settings) {
            Ok(()) => self.status = Some("Settings saved".to_owned()),
            Err(error) => self.status = Some(format!("Could not save settings: {error}")),
        }
    }

    fn schedule_settings_save(&mut self, ctx: &egui::Context) {
        self.settings_save_due_at = Some(self.now() + SETTINGS_SAVE_DEBOUNCE);
        ctx.request_repaint_after(SETTINGS_SAVE_DEBOUNCE);
    }

    fn flush_settings_save_if_due(&mut self, ctx: &egui::Context) {
        let Some(due_at) = self.settings_save_due_at else {
            return;
        };
        let now = self.now();
        if now >= due_at {
            self.save_settings();
        } else {
            ctx.request_repaint_after(due_at - now);
        }
    }

    fn process_command(&mut self, ctx: &egui::Context, command: AppCommand) {
        match command {
            AppCommand::OpenSettings => {
                self.settings_open = true;
                self.update_preview_texture(ctx);
                self.update_root_visibility(ctx);
                self.initialize_settings_window(ctx);
                send_root_command(ctx, egui::ViewportCommand::Focus);
            }
            AppCommand::RunInBackground => {
                #[cfg(target_os = "linux")]
                match crate::linux_daemon::spawn_background_takeover() {
                    Ok(()) => {
                        self.quitting = true;
                        send_root_command(ctx, egui::ViewportCommand::Close);
                    }
                    Err(error) => {
                        self.status = Some(format!("Could not continue in background: {error}"));
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    self.settings_open = false;
                    self.update_root_visibility(ctx);
                }
            }
            AppCommand::ShowNow => {
                let action = self.scheduler.show_now(self.now());
                self.apply_scheduler_action(ctx, action);
            }
            AppCommand::ToggleReminders => {
                self.settings.reminders_enabled = !self.settings.reminders_enabled;
                self.reset_schedule(ctx);
                self.save_settings();
                #[cfg(windows)]
                if let Some(tray) = &self.tray {
                    tray.set_reminders_enabled(self.settings.reminders_enabled);
                }
            }
            AppCommand::DisplayTopologyChanged => {
                #[cfg(windows)]
                self.refresh_displays(ctx);
                #[cfg(target_os = "linux")]
                self.refresh_displays(ctx);
            }
            AppCommand::SessionNotificationsDelayed => {
                self.session_notification_status =
                    Some("Waiting for Windows lock and unlock notifications...".to_owned());
            }
            AppCommand::SessionNotificationsReady => {
                self.session_notification_status = None;
            }
            AppCommand::SessionNotificationsUnavailable(code) => {
                let error = std::io::Error::from_raw_os_error(code as i32);
                self.session_notification_status = Some(format!(
                    "Lock and unlock monitoring is unavailable: {error}"
                ));
            }
            AppCommand::SystemPause(reason) => {
                let was_active = self.system_pause_mask != 0;
                self.system_pause_mask |= pause_reason_mask(reason);
                if !was_active {
                    let action = self.scheduler.begin_break(self.now());
                    self.apply_scheduler_action(ctx, action);
                }
            }
            AppCommand::SystemResume(reason) => {
                self.system_pause_mask &= !pause_reason_mask(reason);
                if self.system_pause_mask == 0 {
                    self.scheduler
                        .end_break(self.now(), self.settings.resume_policy);
                    self.update_root_visibility(ctx);
                }
            }
            AppCommand::Quit => {
                if self.settings_save_due_at.is_some() {
                    self.save_settings();
                }
                self.quitting = true;
                send_root_command(ctx, egui::ViewportCommand::Close);
            }
        }
    }

    #[cfg(windows)]
    fn refresh_displays(&mut self, ctx: &egui::Context) {
        let selected_id = self
            .displays
            .get(self.selected_display)
            .map(|display| display.id.clone());
        let displays: Vec<DisplayInfo> = match crate::windows::enumerate_displays() {
            Ok(displays) => displays.into_iter().map(DisplayInfo::from).collect(),
            Err(error) => {
                append_status(
                    &mut self.status,
                    format!("Could not refresh displays: {error}"),
                );
                return;
            }
        };
        let exact_match = selected_id
            .as_ref()
            .and_then(|id| displays.iter().position(|display| &display.id == id));
        let legacy_match = selected_id
            .as_ref()
            .and_then(|id| displays.iter().position(|display| &display.legacy_id == id));
        let selected_display = exact_match.or(legacy_match).unwrap_or(0);
        let display_id_migrated = exact_match.is_none() && legacy_match.is_some();
        let selection_was_removed =
            selected_id.is_some() && exact_match.is_none() && legacy_match.is_none();

        self.displays = displays;
        self.selected_display = selected_display;
        if display_id_migrated {
            self.save_settings();
        } else if selection_was_removed {
            self.save_settings();
            append_status(
                &mut self.status,
                "Selected display was disconnected; switched to the primary display".to_owned(),
            );
        }
        self.refresh_overlay(ctx);
    }

    #[cfg(target_os = "linux")]
    fn refresh_displays(&mut self, ctx: &egui::Context) {
        let selected_id = self
            .displays
            .get(self.selected_display)
            .map(|display| display.id.clone());
        let Some(overlay) = &self.overlay else {
            return;
        };
        let displays: Vec<DisplayInfo> = match overlay.displays() {
            Ok(displays) => displays.into_iter().map(DisplayInfo::from).collect(),
            Err(error) => {
                append_status(
                    &mut self.status,
                    format!("Could not refresh Wayland outputs: {error}"),
                );
                return;
            }
        };
        if displays.is_empty() {
            append_status(
                &mut self.status,
                "Wayland compositor reported no outputs".to_owned(),
            );
            return;
        }
        let selected_display = selected_id
            .as_ref()
            .and_then(|id| displays.iter().position(|display| &display.id == id))
            .unwrap_or(0);
        let selection_was_removed = selected_id.is_some()
            && selected_id
                .as_ref()
                .is_none_or(|id| displays.iter().all(|display| &display.id != id));
        self.displays = displays;
        self.selected_display = selected_display;
        if selection_was_removed {
            self.save_settings();
            append_status(
                &mut self.status,
                "Selected output was disconnected; switched to the first available output"
                    .to_owned(),
            );
        }
        self.refresh_overlay(ctx);
    }

    fn refresh_tray(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            if self.settings.show_tray_icon && self.tray.is_none() {
                match crate::windows::TrayController::create(self.settings.reminders_enabled) {
                    Ok(tray) => {
                        tray.install_handler(self.commands_tx.clone(), ctx.clone());
                        self.tray = Some(tray);
                    }
                    Err(error) => {
                        self.status = Some(format!("Could not create tray icon: {error}"))
                    }
                }
            } else if !self.settings.show_tray_icon {
                self.tray = None;
            }
        }
        #[cfg(not(windows))]
        let _ = ctx;
    }

    fn choose_image(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "svg"])
            .pick_file()
        else {
            return;
        };
        match load_file(&path) {
            Ok(image) => {
                self.animation_frame_index = 0;
                self.texture
                    .set(image.color_image.clone(), egui::TextureOptions::LINEAR);
                self.image = image;
                self.settings.image_path = Some(path);
                self.save_settings();
                self.refresh_overlay(ctx);
                ctx.request_repaint();
            }
            Err(error) => self.status = Some(format!("Could not load image: {error}")),
        }
    }

    fn restore_default_image(&mut self, ctx: &egui::Context) {
        if let Ok(image) = load_default() {
            self.animation_frame_index = 0;
            self.texture
                .set(image.color_image.clone(), egui::TextureOptions::LINEAR);
            self.image = image;
            self.settings.image_path = None;
            self.save_settings();
            self.refresh_overlay(ctx);
            ctx.request_repaint();
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("OhMyEyes");
        ui.label("A calm, click-through reminder for the 20-20-20 rule.");
        ui.add_space(12.0);

        let mut schedule_changed = false;
        ui.group(|ui| {
            ui.heading("Reminder");
            schedule_changed |= ui
                .checkbox(&mut self.settings.reminders_enabled, "Enable reminders")
                .changed();
            ui.horizontal(|ui| {
                ui.label("Every");
                schedule_changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.interval_minutes)
                            .range(1..=1_440)
                            .suffix(" min"),
                    )
                    .changed();
                ui.label("show for");
                schedule_changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.settings.duration_seconds)
                            .range(1..=600)
                            .suffix(" sec"),
                    )
                    .changed();
            });
            if ui.button("Show reminder now").clicked() {
                let action = self.scheduler.show_now(self.now());
                self.apply_scheduler_action(ui.ctx(), action);
            }
        });
        ui.add_space(8.0);

        let mut visual_changed = false;
        ui.group(|ui| {
            ui.heading("Appearance");
            visual_changed |= ui
                .add(
                    egui::Slider::new(&mut self.settings.opacity_percent, 5..=100)
                        .text("Opacity %"),
                )
                .changed();
            visual_changed |= ui
                .add(egui::Slider::new(&mut self.settings.width_percent, 5..=100).text("Width %"))
                .changed();
            ui.horizontal(|ui| {
                if ui.button("Choose image...").clicked() {
                    self.choose_image(ui.ctx());
                }
                if ui.button("Use default").clicked() {
                    self.restore_default_image(ui.ctx());
                }
            });
            let image_label = self
                .settings
                .resolve_image_path(&self.executable_dir)
                .map_or_else(
                    || "Bundled eye".to_owned(),
                    |path| path.display().to_string(),
                );
            ui.small(image_label);
        });
        ui.add_space(8.0);

        let mut system_changed = false;
        ui.group(|ui| {
            ui.heading("Display and system");
            let selected_label = self
                .displays
                .get(self.selected_display)
                .map_or("Primary display", |display| display.label.as_str());
            egui::ComboBox::from_label("Display")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    for (index, display) in self.displays.iter().enumerate() {
                        system_changed |= ui
                            .selectable_value(&mut self.selected_display, index, &display.label)
                            .changed();
                    }
                });
            system_changed |= ui
                .checkbox(&mut self.settings.start_at_login, "Start at login")
                .changed();
            #[cfg(windows)]
            {
                system_changed |= ui
                    .checkbox(&mut self.settings.show_tray_icon, "Show tray icon")
                    .changed();
            }
            #[cfg(not(windows))]
            ui.add_enabled(
                false,
                egui::Checkbox::new(&mut false, "Show tray icon (Windows only)"),
            );
            #[cfg(windows)]
            ui.horizontal(|ui| {
                ui.label("After sleep or lock");
                system_changed |= ui
                    .selectable_value(
                        &mut self.settings.resume_policy,
                        ResumePolicy::Reset,
                        "Reset interval",
                    )
                    .changed();
                system_changed |= ui
                    .selectable_value(
                        &mut self.settings.resume_policy,
                        ResumePolicy::Continue,
                        "Continue",
                    )
                    .changed();
            });
            #[cfg(not(windows))]
            ui.small("Sleep and session-lock integration is not available on Linux yet.");
            if let Some(status) = &self.session_notification_status {
                ui.small(status);
            }
        });

        if schedule_changed {
            self.reset_schedule(ui.ctx());
        }
        if visual_changed || schedule_changed {
            self.schedule_settings_save(ui.ctx());
        }
        if visual_changed {
            self.refresh_overlay(ui.ctx());
        }
        if system_changed {
            self.apply_system_settings(ui.ctx());
        }

        ui.add_space(12.0);
        ui.label("Drag the eye in the preview to choose its position.");
        let preview_size = egui::vec2(ui.available_width(), 190.0);
        let (response, painter) = ui.allocate_painter(preview_size, egui::Sense::drag());
        painter.rect_filled(response.rect, 12.0, egui::Color32::from_rgb(13, 42, 48));
        painter.rect_stroke(
            response.rect,
            12.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(53, 118, 121)),
            egui::StrokeKind::Inside,
        );
        if let Some(pointer) = response
            .interact_pointer_pos()
            .filter(|_| response.dragged())
        {
            self.settings.position.x =
                ((pointer.x - response.rect.left()) / response.rect.width()).clamp(0.0, 1.0);
            self.settings.position.y =
                ((pointer.y - response.rect.top()) / response.rect.height()).clamp(0.0, 1.0);
            self.schedule_settings_save(ui.ctx());
            self.refresh_overlay(ui.ctx());
        }
        paint_eye(
            &painter,
            response.rect,
            &self.texture,
            self.image.aspect_ratio,
            self.settings.width_percent,
            self.settings.opacity_percent,
            self.settings.position.x,
            self.settings.position.y,
        );
        ui.horizontal(|ui| {
            if ui.button("Center position").clicked() {
                self.settings.position = Default::default();
                self.schedule_settings_save(ui.ctx());
                self.refresh_overlay(ui.ctx());
            }
            if let Some(status) = &self.status {
                ui.small(status);
            }
        });
        ui.add_space(12.0);
        if ui.button("Quit OhMyEyes").clicked() {
            if self.settings_save_due_at.is_some() {
                self.save_settings();
            }
            self.quitting = true;
            send_root_command(ui.ctx(), egui::ViewportCommand::Close);
        }
    }

    fn apply_system_settings(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        match std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|path| {
                crate::windows::set_start_at_login(&path, self.settings.start_at_login)
            }) {
            Ok(()) => {}
            Err(error) => {
                self.settings.start_at_login = false;
                self.status = Some(format!("Could not update start at login: {error}"));
            }
        }
        #[cfg(target_os = "linux")]
        match std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|path| crate::linux::set_start_at_login(&path, self.settings.start_at_login))
        {
            Ok(()) => {}
            Err(error) => {
                self.settings.start_at_login = false;
                self.status = Some(format!("Could not update start at login: {error}"));
            }
        }
        self.refresh_tray(ctx);
        self.update_root_visibility(ctx);
        self.save_settings();
        self.refresh_overlay(ctx);
    }
}

impl eframe::App for OhMyEyesApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        self.initialize_settings_window(ctx);
        while let Ok(command) = self.commands_rx.try_recv() {
            self.process_command(ctx, command);
        }
        #[cfg(windows)]
        if let Some(error) = self
            .overlay
            .as_ref()
            .and_then(crate::windows::OverlayController::take_error)
        {
            append_status(
                &mut self.status,
                format!("Could not render reminder overlay: {error}"),
            );
        }
        #[cfg(target_os = "linux")]
        if let Some(error) = self
            .overlay
            .as_ref()
            .and_then(crate::linux_wayland::OverlayController::take_error)
        {
            append_status(
                &mut self.status,
                format!("Could not render Wayland reminder overlay: {error}"),
            );
        }
        let action = self.scheduler.tick(self.now());
        self.apply_scheduler_action(ctx, action);
        let next_animation_frame = self.update_animation(ctx);
        self.flush_settings_save_if_due(ctx);
        self.update_root_visibility(ctx);
        let next_wake = match (
            self.scheduler.next_wake_in(self.now()),
            next_animation_frame,
        ) {
            (Some(scheduler), Some(animation)) => Some(scheduler.min(animation)),
            (Some(scheduler), None) => Some(scheduler),
            (None, animation) => animation,
        };
        if let Some(wait) = next_wake {
            ctx.request_repaint_after(wait.max(Duration::from_millis(50)));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        let ctx = ui.ctx().clone();
        if self.settings_open
            && !self.quitting
            && ctx.input(|input| input.viewport().close_requested())
        {
            send_root_command(&ctx, egui::ViewportCommand::CancelClose);
            if self.settings_save_due_at.is_some() {
                self.save_settings();
            }
            #[cfg(target_os = "linux")]
            match crate::linux_daemon::spawn_background_takeover() {
                Ok(()) => {
                    self.quitting = true;
                    send_root_command(&ctx, egui::ViewportCommand::Close);
                }
                Err(error) => {
                    self.status = Some(format!("Could not continue in background: {error}"));
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.settings_open = false;
                self.update_root_visibility(&ctx);
            }
        }

        if self.settings_open {
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.settings_ui(ui));
            });
        }

        #[cfg(not(any(windows, target_os = "linux")))]
        if self.scheduler.is_showing() {
            let monitor = self
                .displays
                .get(self.selected_display)
                .map_or(0, |display| display.index);
            ctx.show_viewport_immediate(
                overlay_viewport_id(),
                egui::ViewportBuilder::default()
                    .with_title("OhMyEyes reminder")
                    .with_monitor(monitor)
                    .with_fullscreen(false)
                    .with_maximized(true)
                    .with_transparent(true)
                    .with_decorations(false)
                    .with_mouse_passthrough(true)
                    .with_window_level(egui::WindowLevel::AlwaysOnTop)
                    .with_taskbar(false)
                    .with_active(false),
                |ui, _class| {
                    paint_eye(
                        ui.painter(),
                        ui.max_rect(),
                        &self.texture,
                        self.image.aspect_ratio,
                        self.settings.width_percent,
                        self.settings.opacity_percent,
                        self.settings.position.x,
                        self.settings.position.y,
                    );
                },
            );
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()
    }
}

fn send_root_command(ctx: &egui::Context, command: egui::ViewportCommand) {
    ctx.send_viewport_cmd_to(egui::ViewportId::ROOT, command);
}

fn append_status(status: &mut Option<String>, message: String) {
    if let Some(existing) = status {
        existing.push_str("; ");
        existing.push_str(&message);
    } else {
        *status = Some(message);
    }
}

fn pause_reason_mask(reason: SystemPauseReason) -> u8 {
    match reason {
        SystemPauseReason::Power => 1,
        SystemPauseReason::Session => 2,
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = egui::Color32::from_rgb(244, 239, 224);
    visuals.window_fill = visuals.panel_fill;
    visuals.selection.bg_fill = egui::Color32::from_rgb(16, 112, 112);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(219, 104, 85);
    ctx.set_visuals(visuals);
    let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    ctx.set_style_of(egui::Theme::Light, style);
}

#[cfg(windows)]
impl From<crate::windows::NativeDisplay> for DisplayInfo {
    fn from(display: crate::windows::NativeDisplay) -> Self {
        Self {
            id: display.id,
            legacy_id: display.legacy_id,
            label: display.label,
            left: display.left,
            top: display.top,
            width: display.width,
            height: display.height,
        }
    }
}

#[cfg(target_os = "linux")]
impl From<crate::linux_wayland::NativeDisplay> for DisplayInfo {
    fn from(display: crate::linux_wayland::NativeDisplay) -> Self {
        Self {
            id: display.id,
            label: display.label,
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
fn enumerate_displays(cc: &CreationContext<'_>) -> Vec<DisplayInfo> {
    let mut displays: Vec<_> = cc
        .winit_window()
        .into_iter()
        .flat_map(|window| window.available_monitors())
        .enumerate()
        .map(|(index, monitor)| {
            let name = monitor
                .name()
                .unwrap_or_else(|| format!("Display {}", index + 1));
            let size = monitor.size();
            let position = monitor.position();
            DisplayInfo {
                id: format!(
                    "{name}:{}x{}@{},{}",
                    size.width, size.height, position.x, position.y
                ),
                label: format!("{name} ({} x {})", size.width, size.height),
                index,
            }
        })
        .collect();
    if displays.is_empty() {
        displays.push(DisplayInfo {
            id: "primary".to_owned(),
            label: "Primary display".to_owned(),
            #[cfg(not(windows))]
            index: 0,
            #[cfg(windows)]
            left: 0,
            #[cfg(windows)]
            top: 0,
            #[cfg(windows)]
            width: 1920,
            #[cfg(windows)]
            height: 1080,
        });
    }
    displays
}

#[allow(clippy::too_many_arguments)]
fn paint_eye(
    painter: &egui::Painter,
    bounds: egui::Rect,
    texture: &egui::TextureHandle,
    aspect_ratio: f32,
    width_percent: u8,
    opacity_percent: u8,
    x: f32,
    y: f32,
) {
    let width = bounds.width() * f32::from(width_percent) / 100.0;
    let height = width / aspect_ratio.max(0.01);
    let center = egui::pos2(
        bounds.left() + x * bounds.width(),
        bounds.top() + y * bounds.height(),
    );
    let rect = egui::Rect::from_center_size(center, egui::vec2(width, height));
    painter.image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::from_white_alpha((u16::from(opacity_percent) * 255 / 100) as u8),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_messages_are_appended_without_losing_context() {
        let mut status = None;

        append_status(&mut status, "first".to_owned());
        append_status(&mut status, "second".to_owned());

        assert_eq!(status.as_deref(), Some("first; second"));
    }

    #[test]
    fn system_pause_reasons_use_independent_mask_bits() {
        let power = pause_reason_mask(SystemPauseReason::Power);
        let session = pause_reason_mask(SystemPauseReason::Session);

        assert_ne!(power, 0);
        assert_ne!(session, 0);
        assert_eq!(power & session, 0);
        assert_eq!((power | session) & !power, session);
    }
}
