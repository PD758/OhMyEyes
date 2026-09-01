use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use auto_launch::{AutoLaunch, WindowsEnableMode};
use eframe::egui;
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream, prelude::*};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
        SelectObject,
    },
    System::LibraryLoader::GetModuleHandleW,
    System::RemoteDesktop::{
        NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
    },
    UI::WindowsAndMessaging::{
        CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HTTRANSPARENT, MONITORINFOF_PRIMARY, MSG,
        PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, RegisterClassW, SW_HIDE,
        SW_SHOWNOACTIVATE, SetWindowLongPtrW, ShowWindow, TranslateMessage, ULW_ALPHA,
        UnregisterClassW, UpdateLayeredWindow, WM_APP, WM_DISPLAYCHANGE, WM_ERASEBKGND,
        WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE,
        WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
        WS_EX_TRANSPARENT, WS_POPUP, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
    },
};

use crate::{AppCommand, SystemPauseReason, image_asset::DEFAULT_EYE_BYTES};

const IPC_NAME: &str = "app.ohmyeyes.desktop.ipc";
const IPC_COMMAND_LIMIT: u64 = 64;
const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const OVERLAY_UPDATE_MESSAGE: u32 = WM_APP + 20;
const MAX_OVERLAY_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct NativeDisplay {
    pub id: String,
    pub label: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

struct EnumeratedDisplay {
    display: NativeDisplay,
    primary: bool,
}

pub fn enumerate_displays() -> io::Result<Vec<NativeDisplay>> {
    let mut displays = Vec::<EnumeratedDisplay>::new();
    let succeeded = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect_display),
            (&raw mut displays) as LPARAM,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    displays.sort_by(|left, right| {
        right
            .primary
            .cmp(&left.primary)
            .then_with(|| left.display.top.cmp(&right.display.top))
            .then_with(|| left.display.left.cmp(&right.display.left))
    });
    if displays.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows reported no active displays",
        ));
    }
    Ok(displays.into_iter().map(|entry| entry.display).collect())
}

unsafe extern "system" fn collect_display(
    monitor: HMONITOR,
    _device_context: HDC,
    _monitor_rect: *mut RECT,
    data: LPARAM,
) -> i32 {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = u32::try_from(std::mem::size_of::<MONITORINFOEXW>())
        .expect("MONITORINFOEXW size fits in u32");
    let info_pointer = (&raw mut info).cast::<MONITORINFO>();
    if unsafe { GetMonitorInfoW(monitor, info_pointer) } == 0 {
        return 1;
    }

    let rect = info.monitorInfo.rcMonitor;
    let Some((width, height)) = display_dimensions(&rect) else {
        return 1;
    };

    let name_length = info
        .szDevice
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(info.szDevice.len());
    let name = String::from_utf16_lossy(&info.szDevice[..name_length]);
    let id = if name.is_empty() {
        format!("display@{},{}", rect.left, rect.top)
    } else {
        name.clone()
    };
    let label_name = name
        .strip_prefix(r"\\.\DISPLAY")
        .map_or_else(|| name.clone(), |number| format!("Display {number}"));
    let label_name = if label_name.is_empty() {
        "Display".to_owned()
    } else {
        label_name
    };
    let entry = EnumeratedDisplay {
        display: NativeDisplay {
            id,
            label: format!("{label_name} ({width} x {height})"),
            left: rect.left,
            top: rect.top,
            width,
            height,
        },
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    };
    // EnumDisplayMonitors invokes the callback synchronously while this vector is alive.
    let Some(displays) = (unsafe { (data as *mut Vec<EnumeratedDisplay>).as_mut() }) else {
        return 0;
    };
    displays.push(entry);
    1
}

fn display_dimensions(rect: &RECT) -> Option<(u32, u32)> {
    let width = rect
        .right
        .checked_sub(rect.left)
        .and_then(|value| u32::try_from(value).ok().filter(|dimension| *dimension > 0))?;
    let height = rect
        .bottom
        .checked_sub(rect.top)
        .and_then(|value| u32::try_from(value).ok().filter(|dimension| *dimension > 0))?;
    Some((width, height))
}

const OVERLAY_CLASS_NAME: &[u16] = &[
    b'O' as u16,
    b'h' as u16,
    b'M' as u16,
    b'y' as u16,
    b'E' as u16,
    b'y' as u16,
    b'e' as u16,
    b's' as u16,
    b'O' as u16,
    b'v' as u16,
    b'e' as u16,
    b'r' as u16,
    b'l' as u16,
    b'a' as u16,
    b'y' as u16,
    0,
];
const OVERLAY_TITLE: &[u16] = &[
    b'O' as u16,
    b'h' as u16,
    b'M' as u16,
    b'y' as u16,
    b'E' as u16,
    b'y' as u16,
    b'e' as u16,
    b's' as u16,
    b' ' as u16,
    b'r' as u16,
    b'e' as u16,
    b'm' as u16,
    b'i' as u16,
    b'n' as u16,
    b'd' as u16,
    b'e' as u16,
    b'r' as u16,
    0,
];

struct OverlayFrame {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    image_rgba: Arc<[u8]>,
    image_size: [u32; 2],
    width_percent: u8,
    opacity_percent: u8,
    position: [f32; 2],
}

enum OverlayCommand {
    Show(OverlayFrame),
    Hide,
    Shutdown,
}

pub struct OverlayController {
    pending: Arc<Mutex<Option<OverlayCommand>>>,
    last_error: Arc<Mutex<Option<String>>>,
    window: isize,
    thread: Option<thread::JoinHandle<()>>,
}

impl OverlayController {
    pub fn create() -> Result<Self, String> {
        let pending = Arc::new(Mutex::new(None));
        let worker_pending = Arc::clone(&pending);
        let last_error = Arc::new(Mutex::new(None));
        let worker_last_error = Arc::clone(&last_error);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("ohmyeyes-overlay".to_owned())
            .spawn(move || unsafe {
                overlay_message_loop(worker_pending, worker_last_error, ready_sender)
            })
            .map_err(|error| error.to_string())?;
        let window = ready_receiver.recv().map_err(|error| error.to_string())??;
        Ok(Self {
            pending,
            last_error,
            window,
            thread: Some(thread),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &self,
        left: i32,
        top: i32,
        width: u32,
        height: u32,
        image_rgba: &Arc<[u8]>,
        image_size: [u32; 2],
        width_percent: u8,
        opacity_percent: u8,
        position: [f32; 2],
    ) -> Result<(), String> {
        self.queue(OverlayCommand::Show(OverlayFrame {
            left,
            top,
            width,
            height,
            image_rgba: Arc::clone(image_rgba),
            image_size,
            width_percent,
            opacity_percent,
            position,
        }))
    }

    pub fn hide(&self) -> Result<(), String> {
        self.queue(OverlayCommand::Hide)
    }

    pub fn take_error(&self) -> Option<String> {
        self.last_error.lock().ok()?.take()
    }

    fn queue(&self, command: OverlayCommand) -> Result<(), String> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "overlay command lock is poisoned".to_owned())?;
        let wake_required = pending.is_none();
        *pending = Some(command);
        if wake_required && let Err(error) = self.wake() {
            *pending = None;
            return Err(error);
        }
        Ok(())
    }

    fn wake(&self) -> Result<(), String> {
        let posted = unsafe { PostMessageW(self.window as HWND, OVERLAY_UPDATE_MESSAGE, 0, 0) };
        if posted == 0 {
            Err(io::Error::last_os_error().to_string())
        } else {
            Ok(())
        }
    }
}

impl Drop for OverlayController {
    fn drop(&mut self) {
        let _ = self.queue(OverlayCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe fn overlay_message_loop(
    pending: Arc<Mutex<Option<OverlayCommand>>>,
    last_error: Arc<Mutex<Option<String>>>,
    ready_sender: mpsc::SyncSender<Result<isize, String>>,
) {
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        let _ = ready_sender.send(Err(io::Error::last_os_error().to_string()));
        return;
    }
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(overlay_window_proc),
        hInstance: instance,
        lpszClassName: OVERLAY_CLASS_NAME.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { RegisterClassW(&raw const window_class) } == 0 {
        let _ = ready_sender.send(Err(io::Error::last_os_error().to_string()));
        return;
    }
    let window = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            OVERLAY_CLASS_NAME.as_ptr(),
            OVERLAY_TITLE.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if window.is_null() {
        let _ = ready_sender.send(Err(io::Error::last_os_error().to_string()));
        unsafe { UnregisterClassW(OVERLAY_CLASS_NAME.as_ptr(), instance) };
        return;
    }
    if ready_sender.send(Ok(window as isize)).is_err() {
        unsafe {
            DestroyWindow(window);
            UnregisterClassW(OVERLAY_CLASS_NAME.as_ptr(), instance);
        }
        return;
    }

    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) };
        if result == -1 {
            let error = io::Error::last_os_error();
            tracing::error!(%error, "native overlay message loop failed");
            break;
        }
        if result == 0 {
            break;
        }
        if message.message == OVERLAY_UPDATE_MESSAGE {
            let mut shutdown = false;
            let command = pending.lock().ok().and_then(|mut command| command.take());
            if let Some(command) = command {
                match command {
                    OverlayCommand::Show(frame) => {
                        if let Err(error) = unsafe { update_layered_overlay(window, &frame) } {
                            if let Ok(mut last_error) = last_error.lock() {
                                *last_error = Some(error.clone());
                            }
                            tracing::error!(%error, "native overlay could not be rendered");
                        }
                    }
                    OverlayCommand::Hide => {
                        unsafe { ShowWindow(window, SW_HIDE) };
                    }
                    OverlayCommand::Shutdown => shutdown = true,
                }
            }
            if shutdown {
                break;
            }
        } else {
            unsafe {
                TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }
    unsafe {
        DestroyWindow(window);
        UnregisterClassW(OVERLAY_CLASS_NAME.as_ptr(), instance);
    }
}

#[derive(Debug)]
struct OverlaySurface {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

fn render_overlay_surface(frame: &OverlayFrame) -> Result<OverlaySurface, String> {
    if frame.width == 0 || frame.height == 0 || frame.image_size.contains(&0) {
        return Err("overlay or image dimensions are empty".to_owned());
    }
    let expected_length =
        usize::try_from(u64::from(frame.image_size[0]) * u64::from(frame.image_size[1]) * 4)
            .map_err(|_| "image dimensions are too large".to_owned())?;
    if frame.image_rgba.len() != expected_length {
        return Err("image pixel buffer has an invalid length".to_owned());
    }
    let overlay_width = i32::try_from(frame.width).map_err(|_| "overlay is too wide".to_owned())?;
    let overlay_height =
        i32::try_from(frame.height).map_err(|_| "overlay is too tall".to_owned())?;
    let canvas_length = overlay_canvas_length(frame.width, frame.height)?;

    let (target_width, target_height) = scaled_overlay_size(frame);
    let source = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        frame.image_size[0],
        frame.image_size[1],
        frame.image_rgba.as_ref(),
    )
    .ok_or_else(|| "image pixel buffer could not be created".to_owned())?;
    let scaled = image::imageops::resize(
        &source,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    let mut canvas = Vec::new();
    canvas
        .try_reserve_exact(canvas_length)
        .map_err(|_| "could not allocate the overlay pixel buffer".to_owned())?;
    canvas.resize(canvas_length, 0_u8);
    let center_x = (frame.position[0].clamp(0.0, 1.0) * frame.width as f32).round() as i64;
    let center_y = (frame.position[1].clamp(0.0, 1.0) * frame.height as f32).round() as i64;
    let origin_x = center_x - i64::from(target_width) / 2;
    let origin_y = center_y - i64::from(target_height) / 2;
    let opacity = u32::from(frame.opacity_percent.min(100)) * 255 / 100;

    for source_y in 0..target_height {
        let destination_y = origin_y + i64::from(source_y);
        if !(0..i64::from(frame.height)).contains(&destination_y) {
            continue;
        }
        for source_x in 0..target_width {
            let destination_x = origin_x + i64::from(source_x);
            if !(0..i64::from(frame.width)).contains(&destination_x) {
                continue;
            }
            let pixel = scaled.get_pixel(source_x, source_y).0;
            let alpha = u32::from(pixel[3]) * opacity / 255;
            let destination_y = u64::try_from(destination_y)
                .map_err(|_| "overlay pixel row is outside the canvas".to_owned())?;
            let destination_x = u64::try_from(destination_x)
                .map_err(|_| "overlay pixel column is outside the canvas".to_owned())?;
            let destination = (destination_y * u64::from(frame.width) + destination_x) * 4;
            let destination = usize::try_from(destination)
                .map_err(|_| "overlay pixel offset is too large".to_owned())?;
            let output = canvas
                .get_mut(destination..destination + 4)
                .ok_or_else(|| "overlay pixel offset is outside the canvas".to_owned())?;
            output[0] = (u32::from(pixel[2]) * alpha / 255) as u8;
            output[1] = (u32::from(pixel[1]) * alpha / 255) as u8;
            output[2] = (u32::from(pixel[0]) * alpha / 255) as u8;
            output[3] = alpha as u8;
        }
    }

    Ok(OverlaySurface {
        width: overlay_width,
        height: overlay_height,
        pixels: canvas,
    })
}

unsafe fn update_layered_overlay(window: HWND, frame: &OverlayFrame) -> Result<(), String> {
    let surface = render_overlay_surface(frame)?;
    let screen_dc = std::ptr::null_mut();
    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if memory_dc.is_null() {
        return Err(io::Error::last_os_error().to_string());
    }
    let mut bitmap_info: BITMAPINFO = unsafe { std::mem::zeroed() };
    bitmap_info.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: surface.width,
        biHeight: -surface.height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    let mut bits = std::ptr::null_mut();
    let bitmap = unsafe {
        CreateDIBSection(
            memory_dc,
            &raw const bitmap_info,
            DIB_RGB_COLORS,
            &raw mut bits,
            std::ptr::null_mut(),
            0,
        )
    };
    if bitmap.is_null() || bits.is_null() {
        let error = io::Error::last_os_error().to_string();
        unsafe {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            DeleteDC(memory_dc);
        }
        return Err(error);
    }
    // A 32-bpp DIB has one four-byte pixel per canvas pixel, with no row padding.
    unsafe {
        std::ptr::copy_nonoverlapping(surface.pixels.as_ptr(), bits.cast(), surface.pixels.len())
    };
    let previous = unsafe { SelectObject(memory_dc, bitmap) };
    if previous.is_null() {
        let error = io::Error::last_os_error().to_string();
        unsafe {
            DeleteObject(bitmap);
            DeleteDC(memory_dc);
        }
        return Err(error);
    }
    let destination = windows_sys::Win32::Foundation::POINT {
        x: frame.left,
        y: frame.top,
    };
    let size = windows_sys::Win32::Foundation::SIZE {
        cx: surface.width,
        cy: surface.height,
    };
    let source_point = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let updated = unsafe {
        UpdateLayeredWindow(
            window,
            screen_dc,
            &raw const destination,
            &raw const size,
            memory_dc,
            &raw const source_point,
            0,
            &raw const blend,
            ULW_ALPHA,
        )
    };
    let update_error = (updated == 0).then(|| io::Error::last_os_error().to_string());
    let restored = unsafe { SelectObject(memory_dc, previous) };
    if restored.is_null() {
        unsafe {
            DeleteDC(memory_dc);
            DeleteObject(bitmap);
        }
        return Err("could not restore the previous GDI bitmap".to_owned());
    }
    unsafe {
        DeleteObject(bitmap);
        DeleteDC(memory_dc);
    }
    if let Some(error) = update_error {
        return Err(error);
    }
    unsafe { ShowWindow(window, SW_SHOWNOACTIVATE) };
    Ok(())
}

fn scaled_overlay_size(frame: &OverlayFrame) -> (u32, u32) {
    let requested_width = (u64::from(frame.width) * u64::from(frame.width_percent) / 100)
        .max(1)
        .min(u64::from(frame.width.max(1))) as u32;
    let requested_height = ((u64::from(requested_width) * u64::from(frame.image_size[1]))
        / u64::from(frame.image_size[0]))
    .max(1);
    if requested_height <= u64::from(frame.height) {
        return (requested_width, requested_height as u32);
    }
    let width = ((u64::from(frame.height) * u64::from(frame.image_size[0]))
        / u64::from(frame.image_size[1]))
    .max(1)
    .min(u64::from(frame.width)) as u32;
    (width, frame.height)
}

fn overlay_canvas_length(width: u32, height: u32) -> Result<usize, String> {
    let bytes = u64::from(width) * u64::from(height) * 4;
    if bytes > MAX_OVERLAY_BYTES {
        return Err("overlay pixel buffer exceeds 256 MiB".to_owned());
    }
    usize::try_from(bytes).map_err(|_| "overlay dimensions are too large".to_owned())
}

unsafe extern "system" fn overlay_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => HTTRANSPARENT as LRESULT,
        WM_ERASEBKGND => 1,
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

pub fn notify_running_instance(command: AppCommand) -> io::Result<()> {
    let message = match command {
        AppCommand::ShowNow => b"show-now\n".as_slice(),
        _ => b"open-settings\n".as_slice(),
    };
    let deadline = Instant::now() + IPC_TIMEOUT;
    loop {
        let name = IPC_NAME.to_ns_name::<GenericNamespaced>()?;
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

pub fn start_ipc_server(sender: Sender<AppCommand>, context: egui::Context) -> io::Result<()> {
    let name = IPC_NAME.to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new().name(name).create_sync()?;
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
                    let app_command = match command.trim() {
                        "open-settings" => Some(AppCommand::OpenSettings),
                        "show-now" => Some(AppCommand::ShowNow),
                        _ => None,
                    };
                    if let Some(app_command) = app_command {
                        let _ = sender.send(app_command);
                    }
                    context.request_repaint();
                }
            }
        })?;
    Ok(())
}

struct SystemEventContext {
    sender: Sender<AppCommand>,
    context: egui::Context,
}

pub fn start_system_event_monitor(
    sender: Sender<AppCommand>,
    context: egui::Context,
) -> io::Result<()> {
    thread::Builder::new()
        .name("ohmyeyes-system-events".to_owned())
        .spawn(move || unsafe { system_event_loop(sender, context) })?;
    Ok(())
}

unsafe fn system_event_loop(sender: Sender<AppCommand>, context: egui::Context) {
    const CLASS_NAME: &[u16] = &[
        b'O' as u16,
        b'h' as u16,
        b'M' as u16,
        b'y' as u16,
        b'E' as u16,
        b'y' as u16,
        b'e' as u16,
        b's' as u16,
        b'S' as u16,
        b'y' as u16,
        b's' as u16,
        b't' as u16,
        b'e' as u16,
        b'm' as u16,
        b'E' as u16,
        b'v' as u16,
        b'e' as u16,
        b'n' as u16,
        b't' as u16,
        b's' as u16,
        0,
    ];
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        let error = io::Error::last_os_error();
        tracing::warn!(%error, "current Windows module handle is unavailable");
        return;
    }
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(system_event_window_proc),
        hInstance: instance,
        lpszClassName: CLASS_NAME.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { RegisterClassW(&raw const window_class) } == 0 {
        let error = io::Error::last_os_error();
        tracing::warn!(%error, "system event window class could not be registered");
        return;
    }

    let state = Box::new(SystemEventContext { sender, context });
    let state_pointer = Box::into_raw(state);
    let window = unsafe {
        CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            CLASS_NAME.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            state_pointer.cast(),
        )
    };
    if window.is_null() {
        let error = io::Error::last_os_error();
        drop(unsafe { Box::from_raw(state_pointer) });
        unsafe { UnregisterClassW(CLASS_NAME.as_ptr(), instance) };
        tracing::warn!(%error, "system event window could not be created");
        return;
    }

    let session_notifications_registered =
        unsafe { WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) } != 0;
    if !session_notifications_registered {
        let error = io::Error::last_os_error();
        tracing::warn!(%error, "Windows session notifications could not be registered");
    }
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) };
        if result == -1 {
            let error = io::Error::last_os_error();
            tracing::error!(%error, "system event message loop failed");
            break;
        }
        if result == 0 {
            break;
        }
        unsafe {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    unsafe {
        if session_notifications_registered {
            WTSUnRegisterSessionNotification(window);
        }
        DestroyWindow(window);
        UnregisterClassW(CLASS_NAME.as_ptr(), instance);
        drop(Box::from_raw(state_pointer));
    }
}

unsafe extern "system" fn system_event_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam as *const CREATESTRUCTW;
        if !create.is_null() {
            let state = unsafe { (*create).lpCreateParams } as isize;
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, state) };
        }
    }
    let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const SystemEventContext;
    if !state.is_null() {
        let command = match (message, wparam as u32) {
            (WM_DISPLAYCHANGE, _) => Some(AppCommand::DisplayTopologyChanged),
            (WM_POWERBROADCAST, PBT_APMSUSPEND) => {
                Some(AppCommand::SystemPause(SystemPauseReason::Power))
            }
            (WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK) => {
                Some(AppCommand::SystemPause(SystemPauseReason::Session))
            }
            (WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC) => {
                Some(AppCommand::SystemResume(SystemPauseReason::Power))
            }
            (WM_WTSSESSION_CHANGE, WTS_SESSION_UNLOCK) => {
                Some(AppCommand::SystemResume(SystemPauseReason::Session))
            }
            _ => None,
        };
        if let Some(command) = command {
            let state = unsafe { &*state };
            let _ = state.sender.send(command);
            state.context.request_repaint();
        }
    }
    if message == WM_NCDESTROY {
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

pub fn set_start_at_login(executable: &Path, enabled: bool) -> Result<(), String> {
    let executable = executable
        .to_str()
        .ok_or_else(|| "the executable path is not valid UTF-8".to_owned())?;
    let auto_launch = AutoLaunch::new(
        "OhMyEyes",
        executable,
        WindowsEnableMode::CurrentUser,
        &["--background"],
    );
    let result = if enabled {
        auto_launch.enable()
    } else {
        auto_launch.disable()
    };
    result.map_err(|error| error.to_string())
}

pub struct TrayController {
    _icon: TrayIcon,
    reminders_item: CheckMenuItem,
    open_id: MenuId,
    show_id: MenuId,
    reminders_id: MenuId,
    quit_id: MenuId,
}

impl TrayController {
    pub fn create(reminders_enabled: bool) -> Result<Self, String> {
        let menu = Menu::new();
        let open = MenuItem::with_id("open", "Open settings", true, None);
        let show = MenuItem::with_id("show", "Show reminder now", true, None);
        let reminders = CheckMenuItem::with_id(
            "reminders",
            "Reminders enabled",
            true,
            reminders_enabled,
            None,
        );
        let quit = MenuItem::with_id("quit", "Quit OhMyEyes", true, None);
        menu.append_items(&[
            &open,
            &show,
            &reminders,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|error| error.to_string())?;

        let rgba = image::load_from_memory(DEFAULT_EYE_BYTES)
            .map_err(|error| error.to_string())?
            .resize_exact(64, 64, image::imageops::FilterType::Lanczos3)
            .into_rgba8();
        let icon = Icon::from_rgba(rgba.into_raw(), 64, 64).map_err(|error| error.to_string())?;
        let tray_icon = TrayIconBuilder::new()
            .with_tooltip("OhMyEyes")
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .build()
            .map_err(|error| error.to_string())?;

        Ok(Self {
            _icon: tray_icon,
            open_id: open.id().clone(),
            show_id: show.id().clone(),
            reminders_id: reminders.id().clone(),
            quit_id: quit.id().clone(),
            reminders_item: reminders,
        })
    }

    pub fn install_handler(&self, sender: Sender<AppCommand>, context: egui::Context) {
        let open_id = self.open_id.clone();
        let show_id = self.show_id.clone();
        let reminders_id = self.reminders_id.clone();
        let quit_id = self.quit_id.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let command = if event.id == open_id {
                Some(AppCommand::OpenSettings)
            } else if event.id == show_id {
                Some(AppCommand::ShowNow)
            } else if event.id == reminders_id {
                Some(AppCommand::ToggleReminders)
            } else if event.id == quit_id {
                Some(AppCommand::Quit)
            } else {
                None
            };
            if let Some(command) = command {
                let _ = sender.send(command);
                context.request_repaint();
            }
        }));
    }

    pub fn set_reminders_enabled(&self, enabled: bool) {
        self.reminders_item.set_checked(enabled);
    }
}

impl Drop for TrayController {
    fn drop(&mut self) {
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tall_overlay_image_is_scaled_to_fit_monitor_height() {
        let frame = overlay_frame([1, 4_096]);

        assert_eq!(scaled_overlay_size(&frame), (1, 1_080));
    }

    #[test]
    fn normal_overlay_image_uses_configured_monitor_width() {
        let frame = overlay_frame([400, 200]);

        assert_eq!(scaled_overlay_size(&frame), (480, 240));
    }

    #[test]
    fn wide_overlay_image_keeps_at_least_one_pixel_of_height() {
        let frame = overlay_frame([4_096, 1]);

        assert_eq!(scaled_overlay_size(&frame), (480, 1));
    }

    #[test]
    fn zero_width_percentage_is_defensive_even_before_settings_normalization() {
        let mut frame = overlay_frame([400, 200]);
        frame.width_percent = 0;

        assert_eq!(scaled_overlay_size(&frame), (1, 1));
    }

    #[test]
    fn matching_aspect_ratio_can_fill_the_monitor() {
        let mut frame = overlay_frame([1_920, 1_080]);
        frame.width_percent = 100;

        assert_eq!(scaled_overlay_size(&frame), (1_920, 1_080));
    }

    #[test]
    fn excessive_width_percentage_and_large_monitor_do_not_overflow() {
        let mut frame = overlay_frame([1, 1]);
        frame.width = u32::MAX;
        frame.width_percent = u8::MAX;

        let (width, height) = scaled_overlay_size(&frame);

        assert_eq!(width, frame.height);
        assert_eq!(height, frame.height);
    }

    #[test]
    fn display_dimensions_support_negative_origins_and_reject_overflow() {
        let secondary = RECT {
            left: -1_920,
            top: -1_080,
            right: 0,
            bottom: 0,
        };
        let overflowing = RECT {
            left: i32::MIN,
            top: 0,
            right: i32::MAX,
            bottom: 1,
        };

        assert_eq!(display_dimensions(&secondary), Some((1_920, 1_080)));
        assert_eq!(display_dimensions(&overflowing), None);
        assert_eq!(
            display_dimensions(&RECT {
                left: 10,
                top: 0,
                right: 10,
                bottom: 1,
            }),
            None
        );
    }

    #[test]
    fn overlay_canvas_allocation_is_bounded() {
        assert_eq!(overlay_canvas_length(1_920, 1_080), Ok(8_294_400));
        assert!(overlay_canvas_length(16_384, 16_384).is_err());
    }

    #[test]
    fn rendered_surface_is_premultiplied_bgra() {
        let mut frame = overlay_frame([1, 1]);
        frame.width = 2;
        frame.height = 2;
        frame.width_percent = 50;
        frame.opacity_percent = u8::MAX;
        frame.image_rgba = Arc::from([100, 50, 25, 128]);

        let surface = render_overlay_surface(&frame).expect("surface should render");

        assert_eq!((surface.width, surface.height), (2, 2));
        assert_eq!(&surface.pixels[..12], &[0; 12]);
        assert_eq!(&surface.pixels[12..], &[12, 25, 50, 128]);
    }

    #[test]
    fn rendered_surface_rejects_invalid_pixel_buffer() {
        let frame = overlay_frame([1, 1]);

        let error = render_overlay_surface(&frame).expect_err("empty pixels should be rejected");

        assert_eq!(error, "image pixel buffer has an invalid length");
    }

    #[test]
    #[ignore = "requires an interactive Windows desktop"]
    fn native_layered_overlay_updates_window_bounds() {
        let overlay = OverlayController::create().expect("overlay window should be created");
        let pixel = Arc::from([255, 255, 255, 255]);
        overlay
            .show(10, 20, 320, 200, &pixel, [1, 1], 25, 55, [0.5, 0.5])
            .expect("overlay update should be queued");

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(error) = overlay.take_error() {
                panic!("overlay worker failed: {error}");
            }
            let mut rect = RECT::default();
            let succeeded = unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(
                    overlay.window as HWND,
                    &raw mut rect,
                )
            };
            if succeeded != 0 && rect.right > rect.left && rect.bottom > rect.top {
                assert_eq!(rect.right - rect.left, 320);
                assert_eq!(rect.bottom - rect.top, 200);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "overlay window remained empty after three seconds"
            );
            thread::sleep(Duration::from_millis(20));
        }

        overlay.hide().expect("overlay should hide");
    }

    fn overlay_frame(image_size: [u32; 2]) -> OverlayFrame {
        OverlayFrame {
            left: 0,
            top: 0,
            width: 1_920,
            height: 1_080,
            image_rgba: Arc::from([]),
            image_size,
            width_percent: 25,
            opacity_percent: 55,
            position: [0.5, 0.5],
        }
    }
}
