#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{error::Error, path::PathBuf};

use eframe::egui;
use ohmyeyes::{app::OhMyEyesApp, image_asset};

fn main() -> Result<(), Box<dyn Error>> {
    init_logging();
    let background = std::env::args().any(|argument| argument == "--background");
    let show_now = std::env::args().any(|argument| argument == "--show-now");

    #[cfg(windows)]
    let _instance = {
        let instance = single_instance::SingleInstance::new("app.ohmyeyes.desktop")?;
        if !instance.is_single() {
            let command = if show_now {
                ohmyeyes::AppCommand::ShowNow
            } else {
                ohmyeyes::AppCommand::OpenSettings
            };
            ohmyeyes::windows::notify_running_instance(command)?;
            return Ok(());
        }
        instance
    };

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
        Box::new(move |cc| Ok(Box::new(OhMyEyesApp::new(cc, background)))),
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
