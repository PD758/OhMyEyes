use std::{fs, io, path::Path};

use directories::BaseDirs;
use tempfile::NamedTempFile;

const AUTOSTART_FILE_NAME: &str = "ohmyeyes.desktop";

pub fn set_start_at_login(executable: &Path, enabled: bool) -> Result<(), String> {
    let base =
        BaseDirs::new().ok_or_else(|| "user configuration directory is unavailable".to_owned())?;
    let autostart_directory = base.config_dir().join("autostart");
    set_start_at_login_in(executable, enabled, &autostart_directory)
}

fn set_start_at_login_in(
    executable: &Path,
    enabled: bool,
    autostart_directory: &Path,
) -> Result<(), String> {
    let desktop_file = autostart_directory.join(AUTOSTART_FILE_NAME);
    if !enabled {
        return match fs::remove_file(&desktop_file) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        };
    }

    fs::create_dir_all(autostart_directory).map_err(|error| error.to_string())?;
    let executable = desktop_exec_argument(executable)?;
    let contents = format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=OhMyEyes\nComment=20-20-20 eye-break reminder\nExec={executable} --background\nTerminal=false\nStartupNotify=false\nX-GNOME-Autostart-enabled=true\n"
    );
    let mut temporary =
        NamedTempFile::new_in(autostart_directory).map_err(|error| error.to_string())?;
    use std::io::Write as _;
    temporary
        .write_all(contents.as_bytes())
        .map_err(|error| error.to_string())?;
    temporary
        .persist(&desktop_file)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn desktop_exec_argument(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or_else(|| "the executable path is not valid UTF-8".to_owned())?;
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        if matches!(character, '"' | '`' | '$' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('"');
    Ok(escaped)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn desktop_exec_path_is_quoted_and_escaped() {
        let escaped = desktop_exec_argument(Path::new("/tmp/Oh My$Eyes\\app"))
            .expect("path should be valid UTF-8");

        assert_eq!(escaped, "\"/tmp/Oh My\\$Eyes\\\\app\"");
    }

    #[test]
    fn autostart_file_can_be_enabled_replaced_and_disabled() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let autostart = directory.path().join("autostart");
        let desktop_file = autostart.join(AUTOSTART_FILE_NAME);

        set_start_at_login_in(Path::new("/opt/Oh My Eyes/ohmyeyes"), true, &autostart)
            .expect("autostart should be enabled");
        let first = fs::read_to_string(&desktop_file).expect("desktop file should be readable");
        assert!(first.contains("Exec=\"/opt/Oh My Eyes/ohmyeyes\" --background"));

        set_start_at_login_in(Path::new("/opt/ohmyeyes"), true, &autostart)
            .expect("autostart should be replaceable");
        let replaced = fs::read_to_string(&desktop_file).expect("replacement should be readable");
        assert!(replaced.contains("Exec=\"/opt/ohmyeyes\" --background"));

        set_start_at_login_in(Path::new("/opt/ohmyeyes"), false, &autostart)
            .expect("autostart should be disabled");
        assert!(!desktop_file.exists());
    }
}
