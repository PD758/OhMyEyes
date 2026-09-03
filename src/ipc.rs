use std::{
    io::{self, BufRead, BufReader, Read, Write},
    sync::{Arc, Mutex, mpsc::Sender},
    thread,
    time::{Duration, Instant},
};

use eframe::egui;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream, prelude::*};

use crate::AppCommand;

const INSTANCE_NAME: &str = "app.ohmyeyes.desktop";
const IPC_NAME: &str = "app.ohmyeyes.desktop.ipc";
const IPC_COMMAND_LIMIT: u64 = 64;
const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const IPC_BIND_TIMEOUT: Duration = Duration::from_secs(2);

pub fn notify_running_instance(command: AppCommand) -> io::Result<()> {
    notify_endpoint(&platform_scoped_name(IPC_NAME)?, command)
}

fn notify_endpoint(endpoint: &str, command: AppCommand) -> io::Result<()> {
    let message = encode_command(command);
    let deadline = Instant::now() + IPC_TIMEOUT;
    loop {
        let name = endpoint.to_ns_name::<GenericNamespaced>()?;
        match Stream::connect(name) {
            Ok(mut stream) => return stream.write_all(message),
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for primary instance IPC");
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error),
        }
    }
}

pub struct IpcServer {
    context: Arc<Mutex<Option<egui::Context>>>,
}

pub fn instance_name() -> io::Result<String> {
    platform_scoped_name(INSTANCE_NAME)
}

#[cfg(target_os = "linux")]
pub struct InstanceLock {
    _file: std::fs::File,
}

#[cfg(target_os = "linux")]
pub fn try_instance_lock() -> io::Result<Option<InstanceLock>> {
    let runtime_directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is unavailable"))?;
    validate_runtime_directory(&runtime_directory)?;
    try_instance_lock_at(&runtime_directory.join("ohmyeyes.lock"))
}

#[cfg(target_os = "linux")]
fn try_instance_lock_at(path: &std::path::Path) -> io::Result<Option<InstanceLock>> {
    use rustix::fs::{FlockOperation, flock};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(InstanceLock { _file: file })),
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => Ok(None),
        Err(error) => Err(io::Error::from_raw_os_error(error.raw_os_error())),
    }
}

impl IpcServer {
    pub fn attach_context(self, context: egui::Context) {
        context.request_repaint();
        if let Ok(mut slot) = self.context.lock() {
            *slot = Some(context);
        }
    }
}

pub fn start_ipc_server(sender: Sender<AppCommand>) -> io::Result<IpcServer> {
    let endpoint = platform_scoped_name(IPC_NAME)?;
    start_ipc_server_at(&endpoint, sender)
}

fn start_ipc_server_at(endpoint: &str, sender: Sender<AppCommand>) -> io::Result<IpcServer> {
    let deadline = Instant::now() + IPC_BIND_TIMEOUT;
    let listener = loop {
        let name = endpoint.to_ns_name::<GenericNamespaced>()?;
        match ListenerOptions::new().name(name).create_sync() {
            Ok(listener) => break listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    };
    let context = Arc::new(Mutex::new(None::<egui::Context>));
    let worker_context = Arc::clone(&context);
    thread::Builder::new()
        .name("ohmyeyes-ipc".to_owned())
        .spawn(move || {
            for connection in listener.incoming() {
                let Ok(connection) = connection else {
                    continue;
                };
                if connection.set_recv_timeout(Some(IPC_TIMEOUT)).is_err() {
                    continue;
                }
                let mut command = String::new();
                let mut reader = BufReader::new(connection).take(IPC_COMMAND_LIMIT + 1);
                if reader.read_line(&mut command).is_ok()
                    && command.len() <= IPC_COMMAND_LIMIT as usize
                    && command.ends_with('\n')
                {
                    let app_command = decode_command(command.trim());
                    if let Some(app_command) = app_command {
                        let _ = sender.send(app_command);
                    }
                    if let Ok(context) = worker_context.lock()
                        && let Some(context) = context.as_ref()
                    {
                        context.request_repaint();
                    }
                }
            }
        })?;
    Ok(IpcServer { context })
}

fn encode_command(command: AppCommand) -> &'static [u8] {
    match command {
        AppCommand::ShowNow => b"show-now\n",
        AppCommand::RunInBackground => b"run-in-background\n",
        _ => b"open-settings\n",
    }
}

fn decode_command(command: &str) -> Option<AppCommand> {
    match command {
        "open-settings" => Some(AppCommand::OpenSettings),
        "run-in-background" => Some(AppCommand::RunInBackground),
        "show-now" => Some(AppCommand::ShowNow),
        _ => None,
    }
}

#[cfg(windows)]
fn platform_scoped_name(base: &str) -> io::Result<String> {
    Ok(base.to_owned())
}

#[cfg(target_os = "linux")]
fn platform_scoped_name(base: &str) -> io::Result<String> {
    let uid = current_user_id()?;
    Ok(format!("{base}.uid-{uid}"))
}

#[cfg(target_os = "linux")]
fn current_user_id() -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(std::fs::metadata("/proc/self")?.uid())
}

#[cfg(target_os = "linux")]
fn validate_runtime_directory(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != current_user_id()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "XDG_RUNTIME_DIR is not a directory owned by the current user",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn instance_and_ipc_names_are_distinct() {
        let instance = instance_name().expect("instance name should resolve");
        let ipc = platform_scoped_name(IPC_NAME).expect("IPC name should resolve");

        assert_ne!(instance, ipc);
        #[cfg(target_os = "linux")]
        assert!(instance.contains(".uid-"));
    }

    #[test]
    fn ipc_commands_round_trip() {
        for command in [
            AppCommand::OpenSettings,
            AppCommand::RunInBackground,
            AppCommand::ShowNow,
        ] {
            let encoded = std::str::from_utf8(encode_command(command))
                .expect("IPC command should be UTF-8")
                .trim();
            assert_eq!(decode_command(encoded), Some(command));
        }
        assert_eq!(decode_command("unknown"), None);
    }

    #[test]
    fn ipc_server_delivers_commands_and_accepts_context() {
        let endpoint = platform_scoped_name(&format!("{IPC_NAME}.test-{}", std::process::id()))
            .expect("test IPC name should resolve");
        let (sender, receiver) = std::sync::mpsc::channel();
        let server = start_ipc_server_at(&endpoint, sender).expect("IPC server should start");
        server.attach_context(egui::Context::default());

        for expected in [
            AppCommand::OpenSettings,
            AppCommand::RunInBackground,
            AppCommand::ShowNow,
        ] {
            notify_endpoint(&endpoint, expected).expect("IPC command should be sent");
            assert_eq!(
                receiver
                    .recv_timeout(IPC_TIMEOUT)
                    .expect("IPC command should be received"),
                expected
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn instance_lock_is_exclusive_releasable_and_close_on_exec() {
        use rustix::io::{FdFlags, fcntl_getfd};

        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("instance.lock");
        let first = try_instance_lock_at(&path)
            .expect("first lock attempt should succeed")
            .expect("first lock should be acquired");
        let flags = fcntl_getfd(&first._file).expect("descriptor flags should be readable");

        assert!(flags.contains(FdFlags::CLOEXEC));
        assert!(
            try_instance_lock_at(&path)
                .expect("second lock attempt should complete")
                .is_none()
        );

        drop(first);
        assert!(
            try_instance_lock_at(&path)
                .expect("lock should be reusable after drop")
                .is_some()
        );
    }
}
