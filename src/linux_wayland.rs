use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread,
    time::Duration,
};

use eframe::egui;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputInfo, OutputState},
    reexports::{
        calloop::{EventLoop, channel},
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
};

use crate::AppCommand;

const MAX_OVERLAY_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct NativeDisplay {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone)]
struct OverlayFrame {
    rgba: Vec<u8>,
    image_size: [u32; 2],
    opacity_percent: u8,
}

enum OverlayCommand {
    Show {
        display_id: String,
        rgba: Vec<u8>,
        image_size: [u32; 2],
        width_percent: u8,
        opacity_percent: u8,
        position: [f32; 2],
    },
    Displays(mpsc::SyncSender<Vec<NativeDisplay>>),
    Hide,
    Shutdown,
}

pub struct OverlayController {
    sender: channel::Sender<OverlayCommand>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl OverlayController {
    pub fn create(
        app_sender: Sender<AppCommand>,
        context: Option<egui::Context>,
    ) -> Result<(Self, Vec<NativeDisplay>), String> {
        let (sender, receiver) = channel::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let last_error = Arc::new(Mutex::new(None));
        let worker_error = Arc::clone(&last_error);
        thread::Builder::new()
            .name("ohmyeyes-wayland-overlay".to_owned())
            .spawn(move || {
                let result = run_overlay_worker(
                    receiver,
                    ready_sender.clone(),
                    Arc::clone(&worker_error),
                    app_sender,
                    context,
                );
                if let Err(error) = result {
                    set_last_error(&worker_error, error.clone());
                    let _ = ready_sender.send(Err(error.clone()));
                    tracing::error!(%error, "Wayland overlay worker stopped");
                }
            })
            .map_err(|error| error.to_string())?;

        let displays = ready_receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "Wayland overlay initialization timed out".to_owned())??;
        Ok((Self { sender, last_error }, displays))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &self,
        display_id: &str,
        rgba: &[u8],
        image_size: [u32; 2],
        width_percent: u8,
        opacity_percent: u8,
        position: [f32; 2],
    ) -> Result<(), String> {
        self.sender
            .send(OverlayCommand::Show {
                display_id: display_id.to_owned(),
                rgba: rgba.to_vec(),
                image_size,
                width_percent,
                opacity_percent,
                position,
            })
            .map_err(|_| "Wayland overlay worker is unavailable".to_owned())
    }

    pub fn hide(&self) -> Result<(), String> {
        self.sender
            .send(OverlayCommand::Hide)
            .map_err(|_| "Wayland overlay worker is unavailable".to_owned())
    }

    pub fn displays(&self) -> Result<Vec<NativeDisplay>, String> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(OverlayCommand::Displays(sender))
            .map_err(|_| "Wayland overlay worker is unavailable".to_owned())?;
        receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| "Wayland output enumeration timed out".to_owned())
    }

    pub fn take_error(&self) -> Option<String> {
        self.last_error.lock().ok()?.take()
    }
}

impl Drop for OverlayController {
    fn drop(&mut self) {
        let _ = self.sender.send(OverlayCommand::Shutdown);
    }
}

struct ActiveOverlay {
    layer: LayerSurface,
    output: wl_output::WlOutput,
    output_id: String,
    logical_size: (u32, u32),
    margin: (i32, i32),
    scale: i32,
    frame: OverlayFrame,
    configured: bool,
    pool: SlotPool,
}

struct WaylandState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    active: Option<ActiveOverlay>,
    app_sender: Sender<AppCommand>,
    context: Option<egui::Context>,
    last_error: Arc<Mutex<Option<String>>>,
    exit: bool,
}

fn run_overlay_worker(
    receiver: channel::Channel<OverlayCommand>,
    ready_sender: mpsc::SyncSender<Result<Vec<NativeDisplay>, String>>,
    last_error: Arc<Mutex<Option<String>>>,
    app_sender: Sender<AppCommand>,
    context: Option<egui::Context>,
) -> Result<(), String> {
    let connection = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, mut event_queue) =
        registry_queue_init(&connection).map_err(|error| error.to_string())?;
    let queue_handle = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &queue_handle)
        .map_err(|error| format!("wl_compositor is unavailable: {error}"))?;
    let layer_shell = LayerShell::bind(&globals, &queue_handle)
        .map_err(|error| format!("wlr-layer-shell is unavailable: {error}"))?;
    let shm = Shm::bind(&globals, &queue_handle)
        .map_err(|error| format!("wl_shm is unavailable: {error}"))?;
    let mut state = WaylandState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &queue_handle),
        compositor,
        layer_shell,
        shm,
        active: None,
        app_sender,
        context,
        last_error: Arc::clone(&last_error),
        exit: false,
    };
    event_queue
        .roundtrip(&mut state)
        .map_err(|error| error.to_string())?;
    event_queue
        .roundtrip(&mut state)
        .map_err(|error| error.to_string())?;
    let displays = state.displays();
    if displays.is_empty() {
        let error = "Wayland compositor reported no outputs".to_owned();
        let _ = ready_sender.send(Err(error.clone()));
        return Err(error);
    }
    ready_sender
        .send(Ok(displays))
        .map_err(|_| "application stopped during Wayland initialization".to_owned())?;

    let mut event_loop: EventLoop<WaylandState> =
        EventLoop::try_new().map_err(|error| error.to_string())?;
    WaylandSource::new(connection, event_queue)
        .insert(event_loop.handle())
        .map_err(|error| error.to_string())?;
    let loop_signal = event_loop.get_signal();
    let command_queue_handle = queue_handle.clone();
    let command_error = Arc::clone(&last_error);
    event_loop
        .handle()
        .insert_source(receiver, move |event, (), state| match event {
            channel::Event::Msg(command) => match command {
                OverlayCommand::Show {
                    display_id,
                    rgba,
                    image_size,
                    width_percent,
                    opacity_percent,
                    position,
                } => {
                    let result = state.show(
                        &command_queue_handle,
                        &display_id,
                        rgba,
                        image_size,
                        width_percent,
                        opacity_percent,
                        position,
                    );
                    if let Err(error) = result {
                        set_last_error(&command_error, error);
                    }
                }
                OverlayCommand::Hide => state.hide(),
                OverlayCommand::Displays(sender) => {
                    let _ = sender.send(state.displays());
                }
                OverlayCommand::Shutdown => {
                    state.exit = true;
                    loop_signal.stop();
                }
            },
            channel::Event::Closed => {
                state.exit = true;
                loop_signal.stop();
            }
        })
        .map_err(|error| error.to_string())?;

    while !state.exit {
        if let Err(error) = event_loop.dispatch(None, &mut state) {
            let message = error.to_string();
            set_last_error(&last_error, message.clone());
            return Err(message);
        }
    }
    state.hide();
    Ok(())
}

fn set_last_error(slot: &Arc<Mutex<Option<String>>>, error: String) {
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(error);
    }
}

impl WaylandState {
    fn displays(&self) -> Vec<NativeDisplay> {
        self.output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .map(|info| NativeDisplay {
                id: output_id(&info),
                label: output_label(&info),
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn show(
        &mut self,
        queue_handle: &QueueHandle<Self>,
        display_id: &str,
        rgba: Vec<u8>,
        image_size: [u32; 2],
        width_percent: u8,
        opacity_percent: u8,
        position: [f32; 2],
    ) -> Result<(), String> {
        let (output, info) = self
            .output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output).map(|info| (output, info)))
            .find(|(_, info)| output_id(info) == display_id)
            .or_else(|| {
                self.output_state
                    .outputs()
                    .find_map(|output| self.output_state.info(&output).map(|info| (output, info)))
            })
            .ok_or_else(|| "Wayland compositor reported no usable output".to_owned())?;
        let monitor_size = logical_output_size(&info)
            .ok_or_else(|| format!("Wayland output {display_id} has no active size"))?;
        let layout = overlay_layout(monitor_size, image_size, width_percent, position)?;
        let scale = if self.compositor.wl_compositor().version() >= 3 {
            info.scale_factor.max(1)
        } else {
            1
        };
        let frame = OverlayFrame {
            rgba,
            image_size,
            opacity_percent,
        };

        if let Some(active) = self.active.as_mut()
            && active.output_id == output_id(&info)
            && active.logical_size == layout.size
            && active.margin == layout.margin
            && active.scale == scale
        {
            active.frame = frame;
            if active.configured {
                render_active(active)?;
            }
            return Ok(());
        }

        self.hide();
        let surface = self.compositor.create_surface(queue_handle);
        if scale > 1 {
            surface.set_buffer_scale(scale);
        }
        let input_region = self
            .compositor
            .wl_compositor()
            .create_region(queue_handle, ());
        surface.set_input_region(Some(&input_region));
        let layer = self.layer_shell.create_layer_surface(
            queue_handle,
            surface,
            Layer::Overlay,
            Some("ohmyeyes-reminder"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(layout.size.0, layout.size.1);
        layer.set_margin(layout.margin.1, 0, 0, layout.margin.0);
        layer.commit();
        input_region.destroy();

        let bytes = overlay_buffer_len(layout.size, scale)?;
        let pool = SlotPool::new(bytes.max(4), &self.shm).map_err(|error| error.to_string())?;
        self.active = Some(ActiveOverlay {
            layer,
            output,
            output_id: output_id(&info),
            logical_size: layout.size,
            margin: layout.margin,
            scale,
            frame,
            configured: false,
            pool,
        });
        Ok(())
    }

    fn hide(&mut self) {
        if let Some(active) = self.active.take() {
            let surface = active.layer.wl_surface().clone();
            surface.attach(None, 0, 0);
            active.layer.commit();
            drop(active);
            surface.destroy();
        }
    }

    fn notify_outputs_changed(&self) {
        let _ = self.app_sender.send(AppCommand::DisplayTopologyChanged);
        if let Some(context) = &self.context {
            context.request_repaint();
        }
    }
}

fn render_active(active: &mut ActiveOverlay) -> Result<(), String> {
    let scale = u32::try_from(active.scale).map_err(|_| "invalid Wayland scale".to_owned())?;
    let width = active
        .logical_size
        .0
        .checked_mul(scale)
        .ok_or_else(|| "overlay width is too large".to_owned())?;
    let height = active
        .logical_size
        .1
        .checked_mul(scale)
        .ok_or_else(|| "overlay height is too large".to_owned())?;
    let stride = width
        .checked_mul(4)
        .and_then(|value| i32::try_from(value).ok())
        .ok_or_else(|| "overlay stride is too large".to_owned())?;
    let buffer_width = i32::try_from(width).map_err(|_| "overlay width is too large".to_owned())?;
    let buffer_height =
        i32::try_from(height).map_err(|_| "overlay height is too large".to_owned())?;
    let (buffer, canvas) = active
        .pool
        .create_buffer(
            buffer_width,
            buffer_height,
            stride,
            wl_shm::Format::Argb8888,
        )
        .map_err(|error| error.to_string())?;
    render_frame(
        canvas,
        width,
        height,
        &active.frame.rgba,
        active.frame.image_size,
        active.frame.opacity_percent,
    )?;
    if active.layer.wl_surface().version() >= 4 {
        active
            .layer
            .wl_surface()
            .damage_buffer(0, 0, buffer_width, buffer_height);
    } else {
        let logical_width = i32::try_from(active.logical_size.0)
            .map_err(|_| "overlay logical width is too large".to_owned())?;
        let logical_height = i32::try_from(active.logical_size.1)
            .map_err(|_| "overlay logical height is too large".to_owned())?;
        active
            .layer
            .wl_surface()
            .damage(0, 0, logical_width, logical_height);
    }
    buffer
        .attach_to(active.layer.wl_surface())
        .map_err(|error| error.to_string())?;
    active.layer.commit();
    Ok(())
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.notify_outputs_changed();
    }

    fn update_output(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.notify_outputs_changed();
    }

    fn output_destroyed(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let active_was_destroyed = self
            .active
            .as_ref()
            .is_some_and(|active| active.output == output);
        if active_was_destroyed {
            self.hide();
        }
        self.notify_outputs_changed();
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        layer: &LayerSurface,
    ) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.layer == *layer)
            && let Some(active) = self.active.take()
        {
            let surface = active.layer.wl_surface().clone();
            drop(active);
            surface.destroy();
        }
    }

    fn configure(
        &mut self,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        if active.layer != *layer {
            self.active = Some(active);
            return;
        }
        if configure.new_size.0 > 0 && configure.new_size.1 > 0 {
            active.logical_size = configure.new_size;
        }
        active.configured = true;
        if let Err(error) = render_active(&mut active) {
            tracing::error!(%error, "Wayland overlay could not be rendered");
            set_last_error(&self.last_error, error);
        }
        self.active = Some(active);
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Dispatch<wl_region::WlRegion, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_shm!(WaylandState);
delegate_layer!(WaylandState);
delegate_registry!(WaylandState);

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayLayout {
    size: (u32, u32),
    margin: (i32, i32),
}

fn output_id(info: &OutputInfo) -> String {
    info.name
        .clone()
        .unwrap_or_else(|| format!("wayland-output-{}", info.id))
}

fn output_label(info: &OutputInfo) -> String {
    let name = info.name.as_deref().unwrap_or("Wayland output");
    let description = info
        .description
        .as_deref()
        .filter(|description| *description != name)
        .unwrap_or(info.model.as_str());
    logical_output_size(info).map_or_else(
        || format!("{name} — {description}"),
        |(width, height)| format!("{name} — {description} ({width} x {height})"),
    )
}

fn logical_output_size(info: &OutputInfo) -> Option<(u32, u32)> {
    info.logical_size
        .and_then(|(width, height)| Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?)))
        .or_else(|| {
            let scale = info.scale_factor.max(1);
            info.modes
                .iter()
                .find(|mode| mode.current)
                .and_then(|mode| {
                    Some((
                        u32::try_from(mode.dimensions.0 / scale).ok()?,
                        u32::try_from(mode.dimensions.1 / scale).ok()?,
                    ))
                })
        })
        .filter(|(width, height)| *width > 0 && *height > 0)
}

fn overlay_layout(
    monitor_size: (u32, u32),
    image_size: [u32; 2],
    width_percent: u8,
    position: [f32; 2],
) -> Result<OverlayLayout, String> {
    if monitor_size.0 == 0 || monitor_size.1 == 0 || image_size[0] == 0 || image_size[1] == 0 {
        return Err("overlay or image dimensions are empty".to_owned());
    }
    let requested_width = (u64::from(monitor_size.0) * u64::from(width_percent) / 100).max(1);
    let requested_height = requested_width * u64::from(image_size[1]) / u64::from(image_size[0]);
    let fit = (requested_width as f64 / f64::from(monitor_size.0))
        .max(requested_height as f64 / f64::from(monitor_size.1))
        .max(1.0);
    let width = ((requested_width as f64 / fit).round() as u64).max(1);
    let height = ((requested_height as f64 / fit).round() as u64).max(1);
    let width = u32::try_from(width).map_err(|_| "overlay width is too large".to_owned())?;
    let height = u32::try_from(height).map_err(|_| "overlay height is too large".to_owned())?;
    let max_left = monitor_size.0.saturating_sub(width);
    let max_top = monitor_size.1.saturating_sub(height);
    let center_x = position[0].clamp(0.0, 1.0) * monitor_size.0 as f32;
    let center_y = position[1].clamp(0.0, 1.0) * monitor_size.1 as f32;
    let left = (center_x - width as f32 / 2.0)
        .round()
        .clamp(0.0, max_left as f32) as i32;
    let top = (center_y - height as f32 / 2.0)
        .round()
        .clamp(0.0, max_top as f32) as i32;
    Ok(OverlayLayout {
        size: (width, height),
        margin: (left, top),
    })
}

fn overlay_buffer_len(size: (u32, u32), scale: i32) -> Result<usize, String> {
    let scale = u64::try_from(scale).map_err(|_| "invalid Wayland scale".to_owned())?;
    let bytes = u64::from(size.0)
        .checked_mul(scale)
        .and_then(|value| value.checked_mul(u64::from(size.1)))
        .and_then(|value| value.checked_mul(scale))
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| "overlay pixel buffer is too large".to_owned())?;
    if bytes > MAX_OVERLAY_BYTES {
        return Err("overlay pixel buffer exceeds 256 MiB".to_owned());
    }
    usize::try_from(bytes).map_err(|_| "overlay pixel buffer is too large".to_owned())
}

fn render_frame(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    source: &[u8],
    image_size: [u32; 2],
    opacity_percent: u8,
) -> Result<(), String> {
    let image = image::RgbaImage::from_raw(image_size[0], image_size[1], source.to_vec())
        .ok_or_else(|| "decoded image buffer has invalid dimensions".to_owned())?;
    let resized =
        image::imageops::resize(&image, width, height, image::imageops::FilterType::Lanczos3);
    let expected = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_| "overlay pixel buffer is too large".to_owned())?;
    if canvas.len() < expected {
        return Err("Wayland shared-memory buffer is smaller than expected".to_owned());
    }
    let (pixels, remainder) = canvas[..expected].as_chunks_mut::<4>();
    debug_assert!(remainder.is_empty());
    for (source, destination) in resized.pixels().zip(pixels) {
        let alpha = u16::from(source[3]) * u16::from(opacity_percent) / 100;
        let premultiply = |channel: u8| (u16::from(channel) * alpha / 255) as u8;
        destination[0] = premultiply(source[2]);
        destination[1] = premultiply(source[1]);
        destination[2] = premultiply(source[0]);
        destination[3] = alpha as u8;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn tall_overlay_is_fitted_to_the_output() {
        let layout = overlay_layout((1_920, 1_080), [1, 4_096], 25, [0.5, 0.5])
            .expect("layout should be valid");

        assert_eq!(layout.size, (1, 1_080));
        assert_eq!(layout.margin, (960, 0));
    }

    #[test]
    fn overlay_position_is_clamped_inside_the_output() {
        let top_left =
            overlay_layout((1_920, 1_080), [2, 1], 25, [0.0, 0.0]).expect("layout should be valid");
        let bottom_right =
            overlay_layout((1_920, 1_080), [2, 1], 25, [1.0, 1.0]).expect("layout should be valid");

        assert_eq!(top_left.margin, (0, 0));
        assert_eq!(bottom_right.margin, (1_440, 840));
    }

    #[test]
    fn rendered_pixels_are_premultiplied_bgra() {
        let mut canvas = [0; 4];
        render_frame(&mut canvas, 1, 1, &[100, 50, 200, 128], [1, 1], 50)
            .expect("frame should render");

        assert_eq!(canvas, [50, 12, 25, 64]);
    }
}
