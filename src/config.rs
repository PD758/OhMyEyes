use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::limited_read::{self, LimitedReadError};

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
const MAX_CONFIG_FILE_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumePolicy {
    Reset,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormalizedPosition {
    pub x: f32,
    pub y: f32,
}

impl Default for NormalizedPosition {
    fn default() -> Self {
        Self { x: 0.5, y: 0.5 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    pub reminders_enabled: bool,
    pub interval_minutes: u32,
    pub duration_seconds: u32,
    pub opacity_percent: u8,
    pub width_percent: u8,
    pub position: NormalizedPosition,
    pub display_id: Option<String>,
    pub image_path: Option<PathBuf>,
    pub start_at_login: bool,
    pub show_tray_icon: bool,
    pub resume_policy: ResumePolicy,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            reminders_enabled: true,
            interval_minutes: 20,
            duration_seconds: 20,
            opacity_percent: 55,
            width_percent: 25,
            position: NormalizedPosition::default(),
            display_id: None,
            image_path: None,
            start_at_login: false,
            show_tray_icon: false,
            resume_policy: ResumePolicy::Reset,
        }
    }
}

impl Settings {
    pub fn normalize(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        normalize_range(
            &mut self.interval_minutes,
            1,
            1_440,
            "interval_minutes",
            &mut warnings,
        );
        normalize_range(
            &mut self.duration_seconds,
            1,
            600,
            "duration_seconds",
            &mut warnings,
        );
        normalize_range(
            &mut self.opacity_percent,
            5,
            100,
            "opacity_percent",
            &mut warnings,
        );
        normalize_range(
            &mut self.width_percent,
            5,
            100,
            "width_percent",
            &mut warnings,
        );

        if !self.position.x.is_finite() || !(0.0..=1.0).contains(&self.position.x) {
            self.position.x = 0.5;
            warnings.push("position.x was invalid and was reset".to_owned());
        }
        if !self.position.y.is_finite() || !(0.0..=1.0).contains(&self.position.y) {
            self.position.y = 0.5;
            warnings.push("position.y was invalid and was reset".to_owned());
        }
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            self.schema_version = CONFIG_SCHEMA_VERSION;
        }

        warnings
    }

    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.interval_minutes) * 60)
    }

    pub fn overlay_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(self.duration_seconds))
    }

    pub fn resolve_image_path(&self, executable_dir: &Path) -> Option<PathBuf> {
        self.image_path.as_ref().map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                executable_dir.join(path)
            }
        })
    }
}

fn normalize_range<T>(value: &mut T, min: T, max: T, name: &str, warnings: &mut Vec<String>)
where
    T: Copy + Ord,
{
    let normalized = (*value).clamp(min, max);
    if normalized != *value {
        *value = normalized;
        warnings.push(format!(
            "{name} was outside its supported range and was clamped"
        ));
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("the configuration schema {found} is newer than supported schema {supported}")]
    UnsupportedSchema { found: u64, supported: u32 },
    #[error("the configuration file is larger than 1 MiB")]
    TooLarge,
    #[error("failed to read or write configuration: {0}")]
    Io(#[from] io::Error),
    #[error("failed to parse or serialize configuration: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to atomically persist configuration: {0}")]
    Persist(#[from] tempfile::PersistError),
}

#[derive(Debug)]
pub struct LoadedSettings {
    pub settings: Settings,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn for_current_user() -> io::Result<Self> {
        let base = BaseDirs::new()
            .ok_or_else(|| io::Error::other("user data directory is unavailable"))?;
        Ok(Self::new(
            base.data_local_dir().join("OhMyEyes").join("config.json"),
        ))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<LoadedSettings, ConfigError> {
        if !self.path.exists() {
            return Ok(LoadedSettings {
                settings: Settings::default(),
                warnings: Vec::new(),
            });
        }

        let bytes = match limited_read::read_file(&self.path, MAX_CONFIG_FILE_SIZE) {
            Ok(bytes) => bytes,
            Err(LimitedReadError::TooLarge) => return Err(ConfigError::TooLarge),
            Err(LimitedReadError::Io(error)) => return Err(ConfigError::Io(error)),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let schema_version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(1);
        if schema_version > u64::from(CONFIG_SCHEMA_VERSION) {
            return Err(ConfigError::UnsupportedSchema {
                found: schema_version,
                supported: CONFIG_SCHEMA_VERSION,
            });
        }

        let mut settings: Settings = serde_json::from_value(value)?;
        let warnings = settings.normalize();
        Ok(LoadedSettings { settings, warnings })
    }

    pub fn save(&self, settings: &Settings) -> Result<(), ConfigError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("configuration path has no parent"))?;
        fs::create_dir_all(parent)?;

        let mut normalized = settings.clone();
        normalized.normalize();
        let serialized = serde_json::to_vec_pretty(&normalized)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(&serialized)?;
        temporary.write_all(b"\n")?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_twenty_twenty_twenty_rule() {
        let settings = Settings::default();
        assert_eq!(settings.interval_minutes, 20);
        assert_eq!(settings.duration_seconds, 20);
        assert_eq!(settings.position, NormalizedPosition { x: 0.5, y: 0.5 });
    }

    #[test]
    fn normalization_clamps_external_values() {
        let mut settings = Settings {
            interval_minutes: 0,
            duration_seconds: 9_999,
            opacity_percent: 0,
            width_percent: 255,
            position: NormalizedPosition {
                x: f32::NAN,
                y: -0.4,
            },
            ..Settings::default()
        };

        let warnings = settings.normalize();
        assert_eq!(settings.interval_minutes, 1);
        assert_eq!(settings.duration_seconds, 600);
        assert_eq!(settings.opacity_percent, 5);
        assert_eq!(settings.width_percent, 100);
        assert_eq!(settings.position, NormalizedPosition::default());
        assert_eq!(warnings.len(), 6);
    }

    #[test]
    fn relative_images_are_resolved_from_the_executable_directory() {
        let settings = Settings {
            image_path: Some(PathBuf::from("images/eye.png")),
            ..Settings::default()
        };
        assert_eq!(
            settings.resolve_image_path(Path::new("C:/Apps/OhMyEyes")),
            Some(PathBuf::from("C:/Apps/OhMyEyes/images/eye.png"))
        );
    }

    #[test]
    fn absolute_image_paths_are_preserved() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let image_path = directory.path().join("eye.png");
        let settings = Settings {
            image_path: Some(image_path.clone()),
            ..Settings::default()
        };

        assert_eq!(
            settings.resolve_image_path(Path::new("unused")),
            Some(image_path)
        );
    }

    #[test]
    fn missing_configuration_returns_defaults() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("missing.json"));

        let loaded = store.load().expect("missing configuration should load");

        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn partial_configuration_uses_defaults_and_reports_normalization() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.json");
        fs::write(&path, r#"{"interval_minutes":0}"#).expect("partial configuration should save");

        let loaded = ConfigStore::new(path)
            .load()
            .expect("partial configuration should load");

        assert_eq!(loaded.settings.interval_minutes, 1);
        assert_eq!(loaded.settings.duration_seconds, 20);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("interval_minutes"));
    }

    #[test]
    fn newer_configuration_schema_is_rejected() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.json");
        let schema = u64::from(CONFIG_SCHEMA_VERSION) + 1;
        fs::write(&path, format!(r#"{{"schema_version":{schema}}}"#))
            .expect("future configuration should save");

        let error = ConfigStore::new(path)
            .load()
            .expect_err("future schema should be rejected");

        assert!(matches!(
            error,
            ConfigError::UnsupportedSchema { found, supported }
                if found == schema && supported == CONFIG_SCHEMA_VERSION
        ));
    }

    #[test]
    fn malformed_configuration_is_reported() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.json");
        fs::write(&path, b"{not-json").expect("invalid configuration should save");

        let error = ConfigStore::new(path)
            .load()
            .expect_err("invalid JSON should be rejected");

        assert!(matches!(error, ConfigError::Json(_)));
    }

    #[test]
    fn configuration_round_trips_atomically() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let settings = Settings {
            interval_minutes: 42,
            ..Settings::default()
        };
        store.save(&settings).expect("settings should save");
        let loaded = store.load().expect("settings should load");
        assert_eq!(loaded.settings, settings);

        let replacement = Settings {
            interval_minutes: 17,
            ..Settings::default()
        };
        store
            .save(&replacement)
            .expect("existing settings should be replaced");
        let loaded = store.load().expect("replacement settings should load");
        assert_eq!(loaded.settings, replacement);
    }

    #[test]
    fn saving_normalizes_a_copy_without_mutating_the_caller() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = ConfigStore::new(directory.path().join("config.json"));
        let settings = Settings {
            interval_minutes: 0,
            opacity_percent: 1,
            ..Settings::default()
        };

        store.save(&settings).expect("settings should save");
        let loaded = store.load().expect("saved settings should load");

        assert_eq!(settings.interval_minutes, 0);
        assert_eq!(settings.opacity_percent, 1);
        assert_eq!(loaded.settings.interval_minutes, 1);
        assert_eq!(loaded.settings.opacity_percent, 5);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn oversized_configuration_is_rejected_before_parsing() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("config.json");
        fs::write(&path, vec![b' '; MAX_CONFIG_FILE_SIZE + 1])
            .expect("oversized test configuration should save");

        let error = ConfigStore::new(path)
            .load()
            .expect_err("oversized configuration should be rejected");
        assert!(matches!(error, ConfigError::TooLarge));
    }
}
