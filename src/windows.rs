use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread,
    time::Duration,
};

use auto_launch::{AutoLaunch, WindowsEnableMode};
use eframe::egui;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::{
    Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
        QueryDisplayConfig,
    },
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
        CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
        EnumDisplayMonitors, GetMonitorInfoW, HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO,
        MONITORINFOEXW, SelectObject,
    },
    System::LibraryLoader::GetModuleHandleW,
    System::RemoteDesktop::{
        NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
    },
    UI::WindowsAndMessaging::{
        CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HTTRANSPARENT, KillTimer,
        MONITORINFOF_PRIMARY, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW,
        RegisterClassW, SW_HIDE, SW_SHOWNOACTIVATE, SetTimer, SetWindowLongPtrW, ShowWindow,
        TranslateMessage, ULW_ALPHA, UnregisterClassW, UpdateLayeredWindow, WM_APP,
        WM_DISPLAYCHANGE, WM_ERASEBKGND, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST,
        WM_POWERBROADCAST, WM_TIMER, WM_WTSSESSION_CHANGE, WNDCLASSW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
    },
};

use crate::{AppCommand, SystemPauseReason, image_asset::DEFAULT_EYE_BYTES};

const OVERLAY_UPDATE_MESSAGE: u32 = WM_APP + 20;
const MAX_OVERLAY_BYTES: u64 = 256 * 1024 * 1024;
const OVERLAY_INIT_TIMEOUT: Duration = Duration::from_secs(3);
const SESSION_REGISTRATION_TIMER_ID: usize = 1;
const SESSION_REGISTRATION_RETRY_MS: u32 = 1_000;
// windows-sys exposes the NT variant but not this Win32 RPC status code.
const RPC_S_INVALID_BINDING_CODE: i32 = 1_702;

#[derive(Debug, Clone)]
pub struct NativeDisplay {
    pub id: String,
    pub legacy_id: String,
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

#[derive(Debug)]
struct DisplayIdentity {
    id: String,
    friendly_name: Option<String>,
}

struct DisplayEnumeration {
    displays: Vec<EnumeratedDisplay>,
    identities: HashMap<String, DisplayIdentity>,
}

pub fn enumerate_displays() -> io::Result<Vec<NativeDisplay>> {
    let identities = match display_config_identities() {
        Ok(identities) => identities,
        Err(error) => {
            tracing::warn!(%error, "stable Windows display identities are unavailable");
            HashMap::new()
        }
    };
    let mut enumeration = DisplayEnumeration {
        displays: Vec::new(),
        identities,
    };
    // SAFETY: the callback context points to `enumeration`, which remains alive and
    // exclusively borrowed for the synchronous duration of EnumDisplayMonitors.
    let succeeded = unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect_display),
            (&raw mut enumeration) as LPARAM,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    enumeration.displays.sort_by(|left, right| {
        right
            .primary
            .cmp(&left.primary)
            .then_with(|| left.display.top.cmp(&right.display.top))
            .then_with(|| left.display.left.cmp(&right.display.left))
    });
    if enumeration.displays.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows reported no active displays",
        ));
    }
    Ok(enumeration
        .displays
        .into_iter()
        .map(|entry| entry.display)
        .collect())
}

fn display_config_identities() -> io::Result<HashMap<String, DisplayIdentity>> {
    let paths = loop {
        let mut path_count = 0;
        let mut mode_count = 0;
        // SAFETY: both output count pointers are valid for writes for this call.
        let result = unsafe {
            GetDisplayConfigBufferSizes(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                &raw mut mode_count,
            )
        };
        if result != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: the vectors are allocated to the counts returned by Windows, and all
        // count and buffer pointers remain valid until QueryDisplayConfig returns.
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &raw mut path_count,
                paths.as_mut_ptr(),
                &raw mut mode_count,
                modes.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if result != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        break paths;
    };

    let mut identities = HashMap::new();
    for path in paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            viewGdiDeviceName: [0; 32],
        };
        // SAFETY: `source` has the required type, size, adapter, and source ID fields,
        // and its header pointer refers to writable storage for the whole structure.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut source.header) } != ERROR_SUCCESS as i32 {
            continue;
        }
        let source_name = wide_string(&source.viewGdiDeviceName);
        if source_name.is_empty() {
            continue;
        }

        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: `target` has the required type, size, adapter, and target ID fields,
        // and its header pointer refers to writable storage for the whole structure.
        if unsafe { DisplayConfigGetDeviceInfo(&raw mut target.header) } != ERROR_SUCCESS as i32 {
            continue;
        }
        let device_path = wide_string(&target.monitorDevicePath);
        if device_path.is_empty() {
            continue;
        }
        let friendly_name = wide_string(&target.monitorFriendlyDeviceName);
        identities.insert(
            source_name.to_lowercase(),
            DisplayIdentity {
                id: format!("display-path:{}", device_path.to_lowercase()),
                friendly_name: (!friendly_name.is_empty()).then_some(friendly_name),
            },
        );
    }
    Ok(identities)
}

fn wide_string(buffer: &[u16]) -> String {
    let length = buffer
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

unsafe extern "system" fn collect_display(
    monitor: HMONITOR,
    _device_context: HDC,
    _monitor_rect: *mut RECT,
    data: LPARAM,
) -> i32 {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let info_pointer = (&raw mut info).cast::<MONITORINFO>();
    // SAFETY: Windows supplied `monitor`; `info_pointer` points to a correctly sized,
    // initialized MONITORINFOEXW whose prefix is MONITORINFO.
    if unsafe { GetMonitorInfoW(monitor, info_pointer) } == 0 {
        return 1;
    }

    let rect = info.monitorInfo.rcMonitor;
    let Some((width, height)) = display_dimensions(&rect) else {
        return 1;
    };

    let name = wide_string(&info.szDevice);
    let legacy_id = if name.is_empty() {
        format!("display@{},{}", rect.left, rect.top)
    } else {
        name.clone()
    };
    // EnumDisplayMonitors invokes the callback synchronously while this context is alive.
    // SAFETY: `data` is the pointer passed by `enumerate_displays`, and
    // EnumDisplayMonitors invokes this callback synchronously before it goes out of scope.
    let Some(enumeration) = (unsafe { (data as *mut DisplayEnumeration).as_mut() }) else {
        return 0;
    };
    let identity = enumeration.identities.get(&name.to_lowercase());
    let id = identity.map_or_else(|| legacy_id.clone(), |identity| identity.id.clone());
    let fallback_label = name
        .strip_prefix(r"\\.\DISPLAY")
        .map_or_else(|| name.clone(), |number| format!("Display {number}"));
    let fallback_label = if fallback_label.is_empty() {
        "Display".to_owned()
    } else {
        fallback_label
    };
    let label_name = identity
        .and_then(|identity| identity.friendly_name.clone())
        .unwrap_or(fallback_label);
    let entry = EnumeratedDisplay {
        display: NativeDisplay {
            id,
            legacy_id,
            label: format!("{label_name} ({width} x {height})"),
            left: rect.left,
            top: rect.top,
            width,
            height,
        },
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    };
    enumeration.displays.push(entry);
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
            .spawn(move || {
                overlay_message_loop(worker_pending, worker_last_error, ready_sender);
            })
            .map_err(|error| error.to_string())?;
        let window = ready_receiver
            .recv_timeout(OVERLAY_INIT_TIMEOUT)
            .map_err(|_| "native overlay initialization timed out".to_owned())??;
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
        // SAFETY: `self.window` is created by the overlay thread and remains valid until
        // this controller sends Shutdown and joins that thread in Drop.
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

fn overlay_message_loop(
    pending: Arc<Mutex<Option<OverlayCommand>>>,
    last_error: Arc<Mutex<Option<String>>>,
    ready_sender: mpsc::SyncSender<Result<isize, String>>,
) {
    // SAFETY: a null module name asks Windows for the module of the current process.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        let _ = ready_sender.send(Err(io::Error::last_os_error().to_string()));
        return;
    }
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(overlay_window_proc),
        hInstance: instance,
        lpszClassName: OVERLAY_CLASS_NAME.as_ptr(),
        ..Default::default()
    };
    // SAFETY: all pointers in `window_class` refer to static NUL-terminated strings,
    // and the callback has the required Windows ABI.
    if unsafe { RegisterClassW(&raw const window_class) } == 0 {
        let _ = ready_sender.send(Err(io::Error::last_os_error().to_string()));
        return;
    }
    // SAFETY: the registered class and title pointers are valid NUL-terminated UTF-16;
    // all optional handles and creation data are intentionally null.
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
        // SAFETY: this thread successfully registered the class above and created no window.
        unsafe { UnregisterClassW(OVERLAY_CLASS_NAME.as_ptr(), instance) };
        return;
    }
    if ready_sender.send(Ok(window as isize)).is_err() {
        // SAFETY: both the window and its class were created by this thread and have not
        // yet been destroyed or unregistered.
        unsafe {
            DestroyWindow(window);
            UnregisterClassW(OVERLAY_CLASS_NAME.as_ptr(), instance);
        }
        return;
    }

    let mut surface_resources = None;
    let mut pixel_buffer = Vec::new();
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is writable for the call; a null HWND intentionally receives
        // all messages owned by this overlay thread.
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
                        if let Err(error) = update_layered_overlay(
                            window,
                            &frame,
                            &mut surface_resources,
                            &mut pixel_buffer,
                        ) {
                            if let Ok(mut last_error) = last_error.lock() {
                                *last_error = Some(error.clone());
                            }
                            tracing::error!(%error, "native overlay could not be rendered");
                        }
                    }
                    OverlayCommand::Hide => {
                        // SAFETY: `window` remains owned by this message-loop thread.
                        unsafe { ShowWindow(window, SW_HIDE) };
                    }
                    OverlayCommand::Shutdown => shutdown = true,
                }
            }
            if shutdown {
                break;
            }
        } else {
            // SAFETY: `message` was initialized by a successful GetMessageW call.
            unsafe {
                TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }
    // SAFETY: teardown runs once on the owning thread after the message loop, while
    // both handles are still valid and no further controller wake can outlive Drop's join.
    unsafe {
        DestroyWindow(window);
        UnregisterClassW(OVERLAY_CLASS_NAME.as_ptr(), instance);
    }
}

#[derive(Debug)]
struct OverlayPlacement {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

fn render_overlay_surface(
    frame: &OverlayFrame,
    pixels: &mut Vec<u8>,
) -> Result<OverlayPlacement, String> {
    if frame.width == 0 || frame.height == 0 || frame.image_size.contains(&0) {
        return Err("overlay or image dimensions are empty".to_owned());
    }
    let expected_length =
        usize::try_from(u64::from(frame.image_size[0]) * u64::from(frame.image_size[1]) * 4)
            .map_err(|_| "image dimensions are too large".to_owned())?;
    if frame.image_rgba.len() != expected_length {
        return Err("image pixel buffer has an invalid length".to_owned());
    }
    let (target_width, target_height) = scaled_overlay_size(frame);
    overlay_surface_length(target_width, target_height)?;
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
    let center_x = (frame.position[0].clamp(0.0, 1.0) * frame.width as f32).round() as i64;
    let center_y = (frame.position[1].clamp(0.0, 1.0) * frame.height as f32).round() as i64;
    let origin_x = center_x - i64::from(target_width) / 2;
    let origin_y = center_y - i64::from(target_height) / 2;
    let visible_left = origin_x.max(0);
    let visible_top = origin_y.max(0);
    let visible_right = (origin_x + i64::from(target_width)).min(i64::from(frame.width));
    let visible_bottom = (origin_y + i64::from(target_height)).min(i64::from(frame.height));
    let visible_width = u32::try_from(visible_right - visible_left)
        .map_err(|_| "overlay has no visible width".to_owned())?;
    let visible_height = u32::try_from(visible_bottom - visible_top)
        .map_err(|_| "overlay has no visible height".to_owned())?;
    if visible_width == 0 || visible_height == 0 {
        return Err("overlay is outside the selected display".to_owned());
    }
    let surface_length = overlay_surface_length(visible_width, visible_height)?;
    pixels.clear();
    pixels
        .try_reserve_exact(surface_length)
        .map_err(|_| "could not allocate the overlay pixel buffer".to_owned())?;
    pixels.resize(surface_length, 0_u8);
    let source_left = u32::try_from(visible_left - origin_x)
        .map_err(|_| "overlay source offset is invalid".to_owned())?;
    let source_top = u32::try_from(visible_top - origin_y)
        .map_err(|_| "overlay source offset is invalid".to_owned())?;
    let opacity = u32::from(frame.opacity_percent.min(100)) * 255 / 100;

    for (index, output) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let index = u32::try_from(index).map_err(|_| "overlay pixel index is too large")?;
        let source_x = source_left + index % visible_width;
        let source_y = source_top + index / visible_width;
        let pixel = scaled.get_pixel(source_x, source_y).0;
        let alpha = u32::from(pixel[3]) * opacity / 255;
        output[0] = (u32::from(pixel[2]) * alpha / 255) as u8;
        output[1] = (u32::from(pixel[1]) * alpha / 255) as u8;
        output[2] = (u32::from(pixel[0]) * alpha / 255) as u8;
        output[3] = alpha as u8;
    }

    Ok(OverlayPlacement {
        left: i32::try_from(i64::from(frame.left) + visible_left)
            .map_err(|_| "overlay horizontal position is outside Windows coordinates")?,
        top: i32::try_from(i64::from(frame.top) + visible_top)
            .map_err(|_| "overlay vertical position is outside Windows coordinates")?,
        width: i32::try_from(visible_width).map_err(|_| "overlay is too wide")?,
        height: i32::try_from(visible_height).map_err(|_| "overlay is too tall")?,
    })
}

struct GdiSurface {
    memory_dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
    length: usize,
}

impl GdiSurface {
    fn create(width: i32, height: i32, length: usize) -> Result<Self, String> {
        // SAFETY: a null source DC is explicitly supported for a memory DC; the returned
        // handle is checked before use and owned by the resulting GdiSurface.
        let memory_dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        if memory_dc.is_null() {
            return Err(io::Error::last_os_error().to_string());
        }
        let bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: `bitmap_info` is a valid 32-bit top-down DIB description, `bits` is a
        // writable out-pointer, and both GDI handles are checked before use.
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
            // SAFETY: only non-null handles returned by the calls above are released,
            // and neither has been selected into another DC.
            unsafe {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                DeleteDC(memory_dc);
            }
            return Err(error);
        }
        // SAFETY: `memory_dc` and `bitmap` are valid handles owned by this function.
        let previous = unsafe { SelectObject(memory_dc, bitmap) };
        if previous.is_null() {
            let error = io::Error::last_os_error().to_string();
            // SAFETY: selection failed, so the bitmap is not selected; both handles are
            // valid and released exactly once on this error path.
            unsafe {
                DeleteObject(bitmap);
                DeleteDC(memory_dc);
            }
            return Err(error);
        }
        Ok(Self {
            memory_dc,
            bitmap,
            previous,
            bits: bits.cast(),
            width,
            height,
            length,
        })
    }

    fn matches(&self, surface: &OverlayPlacement, pixel_length: usize) -> bool {
        self.width == surface.width && self.height == surface.height && self.length == pixel_length
    }

    fn copy_pixels(&self, pixels: &[u8]) -> Result<(), String> {
        if self.length != pixels.len() {
            return Err("GDI surface and pixel buffer lengths differ".to_owned());
        }
        // SAFETY: `bits` points to a DIB allocation of exactly `self.length` bytes,
        // established by `create`, and the source slice has the same checked length.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), self.bits, pixels.len()) };
        Ok(())
    }
}

impl Drop for GdiSurface {
    fn drop(&mut self) {
        // SAFETY: all handles are private, were created together, and Drop runs once.
        // Restoring the previous object before deletion follows the GDI ownership rules.
        unsafe {
            let restored = SelectObject(self.memory_dc, self.previous);
            if restored.is_null() {
                DeleteDC(self.memory_dc);
                DeleteObject(self.bitmap);
            } else {
                DeleteObject(self.bitmap);
                DeleteDC(self.memory_dc);
            }
        }
    }
}

fn update_layered_overlay(
    window: HWND,
    frame: &OverlayFrame,
    resources: &mut Option<GdiSurface>,
    pixel_buffer: &mut Vec<u8>,
) -> Result<(), String> {
    let surface = render_overlay_surface(frame, pixel_buffer)?;
    if resources
        .as_ref()
        .is_none_or(|resources| !resources.matches(&surface, pixel_buffer.len()))
    {
        *resources = Some(GdiSurface::create(
            surface.width,
            surface.height,
            pixel_buffer.len(),
        )?);
    }
    let resources = resources
        .as_ref()
        .ok_or_else(|| "overlay GDI surface is unavailable".to_owned())?;
    resources.copy_pixels(pixel_buffer)?;
    let destination = windows_sys::Win32::Foundation::POINT {
        x: surface.left,
        y: surface.top,
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
    // SAFETY: `window` is the live layered window; all geometry/blend pointers refer to
    // initialized stack values, and `memory_dc` owns a selected bitmap of matching size.
    let updated = unsafe {
        UpdateLayeredWindow(
            window,
            std::ptr::null_mut(),
            &raw const destination,
            &raw const size,
            resources.memory_dc,
            &raw const source_point,
            0,
            &raw const blend,
            ULW_ALPHA,
        )
    };
    if updated == 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    // SAFETY: `window` remains valid on its owning message-loop thread.
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

fn overlay_surface_length(width: u32, height: u32) -> Result<usize, String> {
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
        // SAFETY: Windows supplied the callback arguments, which are forwarded unchanged.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
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
        .spawn(move || system_event_loop(sender, context))?;
    Ok(())
}

fn system_event_loop(sender: Sender<AppCommand>, context: egui::Context) {
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
    // SAFETY: a null module name asks Windows for the module of the current process.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        let error = io::Error::last_os_error();
        tracing::warn!(%error, "current Windows module handle is unavailable");
        notify_system_event_error(&sender, &context, &error);
        return;
    }
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(system_event_window_proc),
        hInstance: instance,
        lpszClassName: CLASS_NAME.as_ptr(),
        ..Default::default()
    };
    // SAFETY: the class holds static NUL-terminated strings and a callback with the
    // required Windows ABI.
    if unsafe { RegisterClassW(&raw const window_class) } == 0 {
        let error = io::Error::last_os_error();
        tracing::warn!(%error, "system event window class could not be registered");
        notify_system_event_error(&sender, &context, &error);
        return;
    }

    let state = Box::new(SystemEventContext { sender, context });
    let state_pointer = Box::into_raw(state);
    // SAFETY: the class is registered; the title/class strings are static and valid;
    // `state_pointer` remains allocated until window teardown or this error path.
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
        // SAFETY: ownership came from the single Box::into_raw above and was not
        // transferred to a successfully created window.
        let state = unsafe { Box::from_raw(state_pointer) };
        notify_system_event_error(&state.sender, &state.context, &error);
        drop(state);
        // SAFETY: this thread registered the class and no window was created from it.
        unsafe { UnregisterClassW(CLASS_NAME.as_ptr(), instance) };
        tracing::warn!(%error, "system event window could not be created");
        return;
    }
    // SAFETY: CreateWindowExW succeeded and owns the still-allocated Box pointer as
    // window userdata until teardown after the message loop.
    let state = unsafe { &*state_pointer };

    let mut session_notifications_registered = false;
    let mut session_retry_timer_active = false;
    match register_session_notifications(window) {
        SessionRegistration::Registered => {
            session_notifications_registered = true;
            notify_system_event_state(state, AppCommand::SessionNotificationsReady);
        }
        SessionRegistration::Retry => {
            notify_system_event_state(state, AppCommand::SessionNotificationsDelayed);
            // SAFETY: `window` is valid and owned by this thread; the timer ID and callback
            // mode follow SetTimer's window-timer contract.
            session_retry_timer_active = unsafe {
                SetTimer(
                    window,
                    SESSION_REGISTRATION_TIMER_ID,
                    SESSION_REGISTRATION_RETRY_MS,
                    None,
                )
            } != 0;
            if !session_retry_timer_active {
                let error = io::Error::last_os_error();
                tracing::warn!(%error, "Windows session notification retry timer could not start");
                notify_system_event_state(
                    state,
                    AppCommand::SessionNotificationsUnavailable(error_code(&error)),
                );
            }
        }
        SessionRegistration::Failed(error) => {
            tracing::warn!(%error, "Windows session notifications could not be registered");
            notify_system_event_state(
                state,
                AppCommand::SessionNotificationsUnavailable(error_code(&error)),
            );
        }
    }
    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is writable; null HWND receives messages for this thread.
        let result = unsafe { GetMessageW(&raw mut message, std::ptr::null_mut(), 0, 0) };
        if result == -1 {
            let error = io::Error::last_os_error();
            tracing::error!(%error, "system event message loop failed");
            notify_system_event_error(&state.sender, &state.context, &error);
            break;
        }
        if result == 0 {
            break;
        }
        if message.message == WM_TIMER && message.wParam == SESSION_REGISTRATION_TIMER_ID {
            match register_session_notifications(window) {
                SessionRegistration::Registered => {
                    session_notifications_registered = true;
                    session_retry_timer_active = false;
                    // SAFETY: this timer was created for the same valid window and ID.
                    unsafe { KillTimer(window, SESSION_REGISTRATION_TIMER_ID) };
                    notify_system_event_state(state, AppCommand::SessionNotificationsReady);
                }
                SessionRegistration::Retry => {}
                SessionRegistration::Failed(error) => {
                    session_retry_timer_active = false;
                    // SAFETY: this timer was created for the same valid window and ID.
                    unsafe { KillTimer(window, SESSION_REGISTRATION_TIMER_ID) };
                    tracing::warn!(%error, "Windows session notifications could not be registered");
                    notify_system_event_state(
                        state,
                        AppCommand::SessionNotificationsUnavailable(error_code(&error)),
                    );
                }
            }
            continue;
        }
        // SAFETY: `message` was initialized by a successful GetMessageW call.
        unsafe {
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    // SAFETY: teardown happens once on the owning thread. The callback clears userdata
    // during DestroyWindow before the Box is reconstructed and dropped exactly once.
    unsafe {
        if session_retry_timer_active {
            KillTimer(window, SESSION_REGISTRATION_TIMER_ID);
        }
        if session_notifications_registered {
            WTSUnRegisterSessionNotification(window);
        }
        DestroyWindow(window);
        UnregisterClassW(CLASS_NAME.as_ptr(), instance);
        drop(Box::from_raw(state_pointer));
    }
}

enum SessionRegistration {
    Registered,
    Retry,
    Failed(io::Error),
}

fn register_session_notifications(window: HWND) -> SessionRegistration {
    // SAFETY: the caller supplies the live hidden window owned by the system-event thread.
    if unsafe { WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION) } != 0 {
        return SessionRegistration::Registered;
    }
    let error = io::Error::last_os_error();
    if should_retry_session_registration(&error) {
        SessionRegistration::Retry
    } else {
        SessionRegistration::Failed(error)
    }
}

fn should_retry_session_registration(error: &io::Error) -> bool {
    error.raw_os_error() == Some(RPC_S_INVALID_BINDING_CODE)
}

fn error_code(error: &io::Error) -> u32 {
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .unwrap_or_default()
}

fn notify_system_event_state(state: &SystemEventContext, command: AppCommand) {
    let _ = state.sender.send(command);
    state.context.request_repaint();
}

fn notify_system_event_error(
    sender: &Sender<AppCommand>,
    context: &egui::Context,
    error: &io::Error,
) {
    let _ = sender.send(AppCommand::SessionNotificationsUnavailable(error_code(
        error,
    )));
    context.request_repaint();
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
            // SAFETY: Windows guarantees lParam points to CREATESTRUCTW for WM_NCCREATE.
            let state = unsafe { (*create).lpCreateParams } as isize;
            // SAFETY: `window` is the window under creation and GWLP_USERDATA accepts the
            // application-owned pointer value until WM_NCDESTROY.
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, state) };
        }
    }
    // SAFETY: Windows supplied `window`; reading GWLP_USERDATA does not transfer ownership.
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
            // SAFETY: non-null userdata is the live SystemEventContext installed at
            // WM_NCCREATE and cleared before its Box is dropped.
            let state = unsafe { &*state };
            let _ = state.sender.send(command);
            state.context.request_repaint();
        }
    }
    if message == WM_NCDESTROY {
        // SAFETY: clearing userdata on this valid window prevents later stale-pointer reads.
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
    }
    // SAFETY: Windows supplied all callback arguments, which are forwarded unchanged.
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
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Instant;

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
    fn overlay_surface_allocation_is_bounded() {
        assert_eq!(overlay_surface_length(1_920, 1_080), Ok(8_294_400));
        assert!(overlay_surface_length(16_384, 16_384).is_err());
    }

    #[test]
    fn rendered_surface_uses_only_the_visible_image_bounds() {
        let mut frame = overlay_frame([400, 200]);
        frame.image_rgba = vec![255; 400 * 200 * 4].into();
        let mut pixels = Vec::new();

        let surface = render_overlay_surface(&frame, &mut pixels).expect("surface should render");

        assert_eq!((surface.left, surface.top), (720, 420));
        assert_eq!((surface.width, surface.height), (480, 240));
        assert_eq!(pixels.len(), 480 * 240 * 4);
        let allocation = pixels.as_ptr();
        render_overlay_surface(&frame, &mut pixels).expect("surface should render again");
        assert_eq!(pixels.as_ptr(), allocation);
    }

    #[test]
    fn rendered_surface_clips_at_the_selected_display_edge() {
        let mut frame = overlay_frame([400, 200]);
        frame.position = [0.0, 0.0];
        frame.image_rgba = vec![255; 400 * 200 * 4].into();
        let mut pixels = Vec::new();

        let surface = render_overlay_surface(&frame, &mut pixels).expect("surface should render");

        assert_eq!((surface.left, surface.top), (0, 0));
        assert_eq!((surface.width, surface.height), (240, 120));
    }

    #[test]
    fn rendered_surface_is_premultiplied_bgra() {
        let mut frame = overlay_frame([1, 1]);
        frame.width = 2;
        frame.height = 2;
        frame.width_percent = 50;
        frame.opacity_percent = u8::MAX;
        frame.image_rgba = Arc::from([100, 50, 25, 128]);
        let mut pixels = Vec::new();

        let surface = render_overlay_surface(&frame, &mut pixels).expect("surface should render");

        assert_eq!((surface.left, surface.top), (1, 1));
        assert_eq!((surface.width, surface.height), (1, 1));
        assert_eq!(pixels, [12, 25, 50, 128]);
    }

    #[test]
    fn bundled_eye_produces_visible_native_overlay_pixels() {
        let image = crate::image_asset::load_default().expect("bundled eye should decode");
        let mut frame = overlay_frame(image.size);
        frame.image_rgba = Arc::clone(&image.frame_or_first(0).rgba);
        let mut pixels = Vec::new();

        render_overlay_surface(&frame, &mut pixels).expect("surface should render");

        assert!(pixels.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));
        assert!(pixels.len() < frame.width as usize * frame.height as usize * 4);
    }

    #[test]
    fn rendered_surface_rejects_invalid_pixel_buffer() {
        let frame = overlay_frame([1, 1]);
        let mut pixels = Vec::new();

        let error = render_overlay_surface(&frame, &mut pixels)
            .expect_err("empty pixels should be rejected");

        assert_eq!(error, "image pixel buffer has an invalid length");
    }

    #[test]
    fn session_registration_retries_only_the_documented_startup_error() {
        assert!(should_retry_session_registration(
            &io::Error::from_raw_os_error(RPC_S_INVALID_BINDING_CODE)
        ));
        assert!(!should_retry_session_registration(
            &io::Error::from_raw_os_error(5)
        ));
    }

    #[test]
    fn wide_strings_stop_at_the_first_null() {
        assert_eq!(
            wide_string(&[b'A' as u16, b'B' as u16, 0, b'C' as u16]),
            "AB"
        );
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
            // SAFETY: the integration test owns `overlay`; its worker keeps the native
            // window alive until the controller is dropped after this query.
            let succeeded = unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(
                    overlay.window as HWND,
                    &raw mut rect,
                )
            };
            if succeeded != 0 && rect.right > rect.left && rect.bottom > rect.top {
                assert_eq!(rect.left, 130);
                assert_eq!(rect.top, 80);
                assert_eq!(rect.right - rect.left, 80);
                assert_eq!(rect.bottom - rect.top, 80);
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
