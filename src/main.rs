#![cfg_attr(windows, windows_subsystem = "windows")]
#![cfg_attr(not(windows), forbid(unsafe_code))]

use std::{error::Error, path::PathBuf};

use eframe::egui;
use ohmyeyes::{
    app::{AppStartup, OhMyEyesApp},
    image_asset,
};

fn main() -> Result<(), Box<dyn Error>> {
    init_logging();
    let arguments: Vec<_> = std::env::args().collect();
    let show_now = arguments.iter().any(|argument| argument == "--show-now");
    #[cfg(target_os = "linux")]
    let takeover = arguments.iter().any(|argument| {
        argument == ohmyeyes::linux_daemon::BACKGROUND_TAKEOVER_ARGUMENT
            || argument == ohmyeyes::linux_daemon::FOREGROUND_TAKEOVER_ARGUMENT
    });
    let background = arguments.iter().any(|argument| argument == "--background")
        || cfg!(target_os = "linux")
            && arguments
                .iter()
                .any(|argument| argument == "--background-takeover");

    #[cfg(windows)]
    let _instance = {
        let instance_name = ohmyeyes::ipc::instance_name()?;
        let instance = single_instance::SingleInstance::new(&instance_name)?;
        if !instance.is_single() {
            let command = if background {
                ohmyeyes::AppCommand::RunInBackground
            } else if show_now {
                ohmyeyes::AppCommand::ShowNow
            } else {
                ohmyeyes::AppCommand::OpenSettings
            };
            ohmyeyes::ipc::notify_running_instance(command)?;
            return Ok(());
        }
        instance
    };

    #[cfg(target_os = "linux")]
    let _instance = {
        let takeover_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(instance) = ohmyeyes::ipc::try_instance_lock()? {
                break instance;
            }
            if takeover && std::time::Instant::now() < takeover_deadline {
                std::thread::sleep(std::time::Duration::from_millis(25));
                continue;
            }
            let command = if background {
                ohmyeyes::AppCommand::RunInBackground
            } else if show_now {
                ohmyeyes::AppCommand::ShowNow
            } else {
                ohmyeyes::AppCommand::OpenSettings
            };
            ohmyeyes::ipc::notify_running_instance(command)?;
            return Ok(());
        }
    };

    #[cfg(target_os = "linux")]
    if background {
        return ohmyeyes::linux_daemon::run(show_now)
            .map_err(std::io::Error::other)
            .map_err(Into::into);
    }

    let startup = AppStartup::initialize();

    let icon = image::load_from_memory(image_asset::DEFAULT_EYE_BYTES)
        .ok()
        .map(|image| {
            let rgba = image
                .resize_exact(128, 128, image::imageops::FilterType::Lanczos3)
                .into_rgba8();
            egui::IconData {
                rgba: rgba.into_raw(),
                width: 128,
                height: 128,
            }
        });
    let options = eframe::NativeOptions {
        persist_window: false,
        viewport: egui::ViewportBuilder::default()
            .with_title("OhMyEyes settings")
            .with_decorations(true)
            .with_transparent(true)
            .with_inner_size([560.0, 720.0])
            .with_min_inner_size([460.0, 620.0])
            .with_position([100.0, 100.0])
            .with_mouse_passthrough(false)
            .with_taskbar(true)
            .with_active(!background)
            .with_visible(!background)
            .with_icon(icon.unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native(
        "OhMyEyes",
        options,
        Box::new(move |cc| {
            Ok(Box::new(OhMyEyesApp::new(
                cc, background, show_now, startup,
            )?))
        }),
    )?;
    Ok(())
}

fn init_logging() {
    let log_directory = directories::BaseDirs::new()
        .map(|base| base.data_local_dir().join("OhMyEyes").join("logs"))
        .unwrap_or_else(|| PathBuf::from("logs"));
    if std::fs::create_dir_all(&log_directory).is_err() {
        return;
    }
    let Ok(file) = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("ohmyeyes.log")
        .max_log_files(14)
        .build(log_directory)
    else {
        return;
    };
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(file)
        .with_ansi(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}
