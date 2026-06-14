use std::collections::HashMap;
use std::sync::Arc;
use zbus::{interface, connection};
use zbus::zvariant::Value;
use glyphon::{
    Attrs, Buffer, Cache, FontSystem, Metrics, Resolution, SwashCache, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window, delegate_output,
    registry::{ProvidesRegistryState, RegistryState},
    output::{OutputHandler, OutputState},
    seat::{
        keyboard::KeyboardHandler,
        pointer::PointerHandler,
        Capability, SeatHandler, SeatState,
    },
    shell::{
        xdg::{
            window::{Window as XdgWindow, WindowConfigure, WindowHandler, WindowDecorations},
            XdgShell,
        },
        WaylandSurface,
    },
    shm::{Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle, Proxy,
};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;

// ── Vertices and rendering structures ──

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
    clip_circle: [f32; 3],
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x4,
        2 => Float32x3,
    ];

    fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBS,
        }
    }
}

fn quad_vertices(x: f32, y: f32, w: f32, h: f32, sw: f32, sh: f32, c: [f32; 4]) -> [Vertex; 6] {
    let x0 = (x / sw) * 2.0 - 1.0;
    let y0 = 1.0 - (y / sh) * 2.0;
    let x1 = ((x + w) / sw) * 2.0 - 1.0;
    let y1 = 1.0 - ((y + h) / sh) * 2.0;
    [
        Vertex { position: [x0, y0], color: c, clip_circle: [0.0; 3] },
        Vertex { position: [x1, y0], color: c, clip_circle: [0.0; 3] },
        Vertex { position: [x0, y1], color: c, clip_circle: [0.0; 3] },
        Vertex { position: [x1, y0], color: c, clip_circle: [0.0; 3] },
        Vertex { position: [x1, y1], color: c, clip_circle: [0.0; 3] },
        Vertex { position: [x0, y1], color: c, clip_circle: [0.0; 3] },
    ]
}

fn make_text_buffer(fs: &mut FontSystem, text: &str, size: f32) -> Buffer {
    let metrics = Metrics::new(size, size * 1.4);
    let mut buf = Buffer::new(fs, metrics);
    buf.set_text(fs, text, Attrs::new(), glyphon::Shaping::Advanced);
    buf.shape_until_scroll(fs, true);
    buf
}

struct RectWidget {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [f32; 4],
}

struct TextItem {
    buffer: Buffer,
    x: f32,
    y: f32,
    color: glyphon::Color,
}

// ── NotificationApp ──

struct RendererResources {
    render_pipeline: wgpu::RenderPipeline,
    font_system: FontSystem,
    swash_cache: SwashCache,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    cache: Cache,
}

struct NotificationApp {
    window: XdgWindow,
    surface: wl_surface::WlSurface,
    wgpu_surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,

    text_viewport: Viewport,

    rects: Vec<RectWidget>,
    text_items: Vec<TextItem>,

    scale_factor: f64,
    width: u32,
    height: u32,
    needs_rebuild: bool,
    configured: bool,
    opacity: f32,

    app_name: String,
    summary: String,
    body: String,
}

impl NotificationApp {
    fn new(
        conn: &Connection,
        qh: &QueueHandle<AppState>,
        compositor_state: &CompositorState,
        xdg_shell_state: &XdgShell,
        width: u32,
        height: u32,
        scale: f64,
        instance: &wgpu::Instance,
        adapter: &wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        cache: &Cache,
        opacity: f32,
    ) -> Self {
        let surface = compositor_state.create_surface(qh);
        surface.set_buffer_scale(scale as i32);
        let window = xdg_shell_state.create_window(surface.clone(), WindowDecorations::None, qh);
        window.set_title("Notification");
        window.set_app_id("clear-notification-daemon");
        window.set_min_size(Some(((width as f64 / scale) as u32, (height as f64 / scale) as u32)));
        window.set_max_size(Some(((width as f64 / scale) as u32, (height as f64 / scale) as u32)));
        window.commit();

        let wayland_handle = Box::leak(Box::new(clear_ui::wayland::WaylandSurfaceHandle {
            display_ptr: conn.backend().display_id().as_ptr() as *mut std::ffi::c_void,
            surface_ptr: surface.id().as_ptr() as *mut std::ffi::c_void,
        }));

        let wgpu_surface = instance.create_surface(wayland_handle).expect("surface");
        let mut config = wgpu_surface.get_default_config(adapter, width.max(1), height.max(1)).expect("config");
        config.format = wgpu::TextureFormat::Bgra8Unorm;
        
        let capabilities = wgpu_surface.get_capabilities(adapter);
        let alpha_mode = if capabilities.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else if capabilities.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
            wgpu::CompositeAlphaMode::PostMultiplied
        } else {
            capabilities.alpha_modes[0]
        };
        config.alpha_mode = alpha_mode;
        wgpu_surface.configure(&device, &config);

        let mut text_viewport = Viewport::new(&device, cache);
        text_viewport.update(&queue, Resolution { width, height });

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Vertex Buffer"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let scale_factor = scale;

        Self {
            window,
            surface,
            wgpu_surface,
            device,
            queue,
            config,
            vertex_buffer,
            vertex_count: 0,
            text_viewport,
            rects: Vec::new(),
            text_items: Vec::new(),
            scale_factor,
            width,
            height,
            needs_rebuild: true,
            configured: false,
            opacity,
            app_name: String::new(),
            summary: String::new(),
            body: String::new(),
        }
    }

    fn rebuild_layout(&mut self, font_system: &mut FontSystem) {
        let sw = self.width as f32;
        let sh = self.height as f32;
        let s = self.scale_factor as f32;

        self.rects.clear();
        self.text_items.clear();

        // 1. Dark Card Background
        self.rects.push(RectWidget {
            x: 0.0,
            y: 0.0,
            w: sw,
            h: sh,
            color: [
                clear_ui::colors::HEADER_BG[0],
                clear_ui::colors::HEADER_BG[1],
                clear_ui::colors::HEADER_BG[2],
                self.opacity,
            ],
        });

        // 2. Bright Green Left accent border
        self.rects.push(RectWidget {
            x: 0.0,
            y: 0.0,
            w: 6.0 * s,
            h: sh,
            color: clear_ui::colors::TOGGLE_ON,
        });

        // 3. Text content
        let app_name_buf = make_text_buffer(font_system, &self.app_name, 10.0 * s);
        self.text_items.push(TextItem {
            buffer: app_name_buf,
            x: 18.0 * s,
            y: 12.0 * s,
            color: glyphon::Color::rgb(
                (clear_ui::colors::TEXT_DIM[0] * 255.0) as u8,
                (clear_ui::colors::TEXT_DIM[1] * 255.0) as u8,
                (clear_ui::colors::TEXT_DIM[2] * 255.0) as u8,
            ),
        });

        let summary_buf = make_text_buffer(font_system, &self.summary, 13.0 * s);
        self.text_items.push(TextItem {
            buffer: summary_buf,
            x: 18.0 * s,
            y: 28.0 * s,
            color: glyphon::Color::rgb(
                (clear_ui::colors::TEXT_HEADER[0] * 255.0) as u8,
                (clear_ui::colors::TEXT_HEADER[1] * 255.0) as u8,
                (clear_ui::colors::TEXT_HEADER[2] * 255.0) as u8,
            ),
        });

        let body_buf = make_text_buffer(font_system, &self.body, 11.0 * s);
        self.text_items.push(TextItem {
            buffer: body_buf,
            x: 18.0 * s,
            y: 48.0 * s,
            color: glyphon::Color::rgb(
                (clear_ui::colors::TEXT_FG[0] * 255.0) as u8,
                (clear_ui::colors::TEXT_FG[1] * 255.0) as u8,
                (clear_ui::colors::TEXT_FG[2] * 255.0) as u8,
            ),
        });

        self.needs_rebuild = false;
    }

    fn collect_vertices(&self) -> Vec<Vertex> {
        let sw = self.width as f32;
        let sh = self.height as f32;
        let mut verts = Vec::new();
        for r in &self.rects {
            verts.extend(quad_vertices(r.x, r.y, r.w, r.h, sw, sh, r.color));
        }
        verts
    }

    fn upload_vertices(&mut self) {
        let verts = self.collect_vertices();
        self.vertex_count = verts.len() as u32;
        let data = bytemuck::cast_slice(&verts);
        let needed = data.len() as wgpu::BufferAddress;
        if needed > self.vertex_buffer.size() {
            self.vertex_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("Vertex Buffer"),
                size: needed,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue.write_buffer(&self.vertex_buffer, 0, data);
    }

    fn prepare_text(
        &mut self,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        text_atlas: &mut TextAtlas,
        text_renderer: &mut TextRenderer,
    ) {
        let w = self.width as f32;
        let h = self.height as f32;
        let viewport = Resolution { width: w as u32, height: h as u32 };
        self.text_viewport.update(&self.queue, viewport);
        let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        let areas: Vec<TextArea> = self.text_items.iter().map(|ti| TextArea {
            buffer: &ti.buffer,
            left: ti.x.round(), top: ti.y.round(), scale: 1.0, bounds,
            default_color: ti.color,
            custom_glyphs: &[],
        }).collect();
        text_renderer.prepare(
            &self.device, &self.queue, font_system,
            text_atlas, &self.text_viewport, areas, swash_cache
        ).unwrap();
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.width = width;
            self.height = height;
            self.config.width = width;
            self.config.height = height;
            self.wgpu_surface.configure(&self.device, &self.config);
            self.needs_rebuild = true;
        }
    }

    fn render(
        &mut self,
        render_pipeline: &wgpu::RenderPipeline,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        text_atlas: &mut TextAtlas,
        text_renderer: &mut TextRenderer,
    ) {
        if self.needs_rebuild {
            self.rebuild_layout(font_system);
            self.upload_vertices();
        }
        self.prepare_text(font_system, swash_cache, text_atlas, text_renderer);

        let output = match self.wgpu_surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.wgpu_surface.configure(&self.device, &self.config);
                return;
            }
            Err(wgpu::SurfaceError::Timeout) => return,
            Err(e) => { eprintln!("Surface error: {e:?}"); return; }
        };

        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Encoder"),
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_pipeline(render_pipeline);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.draw(0..self.vertex_count, 0..1);

            text_renderer.render(text_atlas, &self.text_viewport, &mut pass).unwrap();
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }
}

fn play_bell_if_configured() {
    let config_path = "/home/lsgalante/.config/cce/config.toml";
    let content = std::fs::read_to_string(config_path).unwrap_or_default();
    
    let mut in_section = false;
    let mut bell_enabled = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[notifications]" {
            in_section = true;
            continue;
        }
        if trimmed.starts_with('[') && in_section {
            break;
        }
        if in_section && trimmed.starts_with("bell") {
            if let Some(val) = trimmed.split('=').nth(1) {
                bell_enabled = val.trim() == "true";
            }
        }
    }
    
    if bell_enabled {
        println!("[cce-notification-daemon] Playing notification bell sound...");
        if let Err(e) = std::process::Command::new("pw-play")
            .arg("/usr/share/sounds/freedesktop/stereo/bell.oga")
            .spawn()
        {
            eprintln!("[cce-notification-daemon] Failed to spawn pw-play: {}", e);
        }
    }
}

fn read_duration_if_configured() -> u64 {
    let config_path = "/home/lsgalante/.config/cce/config.toml";
    let content = std::fs::read_to_string(config_path).unwrap_or_default();
    
    let mut in_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[notifications]" {
            in_section = true;
            continue;
        }
        if trimmed.starts_with('[') && in_section {
            break;
        }
        if in_section && trimmed.starts_with("duration") {
            if let Some(val) = trimmed.split('=').nth(1) {
                if let Ok(d) = val.trim().parse::<u64>() {
                    return d;
                }
            }
        }
    }
    5 // default to 5 seconds
}

fn read_opacity_if_configured() -> f32 {
    let config_path = "/home/lsgalante/.config/cce/config.toml";
    let content = std::fs::read_to_string(config_path).unwrap_or_default();
    
    let mut in_section = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[transparency]" {
            in_section = true;
            continue;
        }
        if trimmed.starts_with('[') && in_section {
            break;
        }
        if in_section && trimmed.starts_with("opacity") {
            if let Some(val) = trimmed.split('=').nth(1) {
                if let Ok(o) = val.trim().parse::<f32>() {
                    return o.clamp(0.0, 1.0);
                }
            }
        }
    }
    0.9 // default opacity
}

// ── D-Bus Events & AppState ──

#[derive(Debug, Clone)]
enum UserEvent {
    NewNotification {
        app_name: String,
        summary: String,
        body: String,
    },
    CloseNotification {
        notification_id: u32,
    },
}

struct AppState {
    registry_state: RegistryState,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShell,
    shm_state: Shm,
    seat_state: SeatState,
    output_state: OutputState,

    seats: Vec<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    state: Option<NotificationApp>,
    current_id: u32,
    exit: bool,
    redraw: bool,

    conn: Connection,
    qh: QueueHandle<AppState>,
    rt_handle: tokio::runtime::Handle,
    sender: calloop::channel::Sender<UserEvent>,

    wgpu_instance: wgpu::Instance,
    wgpu_adapter: wgpu::Adapter,
    wgpu_device: wgpu::Device,
    wgpu_queue: wgpu::Queue,
    renderer_resources: RendererResources,
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        scale_factor: i32,
    ) {
        _surface.set_buffer_scale(scale_factor);
        if let Some(state) = &mut self.state {
            let old_scale = state.scale_factor;
            state.scale_factor = scale_factor as f64;
            let logical_w = state.width as f64 / old_scale;
            let logical_h = state.height as f64 / old_scale;
            let pw = (logical_w * state.scale_factor) as u32;
            let ph = (logical_h * state.scale_factor) as u32;
            state.resize(pw, ph);
        }
        self.redraw = true;
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {}

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {}

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {}

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {}
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seats.push(seat);
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self.seat_state.get_pointer(qh, &seat).unwrap();
            self.pointer = Some(pointer);
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard(qh, &seat, None).unwrap();
            self.keyboard = Some(keyboard);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
        }
        if capability == Capability::Keyboard {
            self.keyboard = None;
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.seats.retain(|s| s != &seat);
    }
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl PointerHandler for AppState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        _events: &[smithay_client_toolkit::seat::pointer::PointerEvent],
    ) {}
}

impl KeyboardHandler for AppState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw_modifiers: &[u32],
        _keysyms: &[xkeysym::Keysym],
    ) {}

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {}

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {}

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {}

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        _layout: u32,
    ) {}
}

impl WindowHandler for AppState {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &XdgWindow,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if let Some(state) = &mut self.state {
            state.configured = true;
            if let (Some(w), Some(h)) = (w, h) {
                let width = w.get();
                let height = h.get();
                let pw = (width as f64 * state.scale_factor) as u32;
                let ph = (height as f64 * state.scale_factor) as u32;
                state.resize(pw, ph);
            }
        }
        self.redraw = true;
    }

    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &XdgWindow) {
        self.state = None;
        self.redraw = true;
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    
    fn runtime_add_global(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _name: u32,
        _interface: &str,
        _version: u32,
    ) {}
    
    fn runtime_remove_global(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _name: u32,
        _interface: &str,
    ) {}
}

delegate_compositor!(AppState);
delegate_xdg_shell!(AppState);
delegate_xdg_window!(AppState);
delegate_shm!(AppState);
delegate_seat!(AppState);
delegate_pointer!(AppState);
delegate_keyboard!(AppState);
delegate_registry!(AppState);
delegate_output!(AppState);

impl AppState {
    fn handle_user_event(&mut self, event: UserEvent) {
        match event {
            UserEvent::NewNotification { app_name, summary, body } => {
                play_bell_if_configured();
                self.current_id += 1;
                let active_id = self.current_id;
                let opacity = read_opacity_if_configured();

                if self.state.is_none() {
                    println!("[cce-notification-daemon] Opening notification window: {} - {}", summary, body);
                    let scale = clear_ui::wayland::detect_scale_factor(&self.output_state);
                    let pw = (360.0 * scale) as u32;
                    let ph = (100.0 * scale) as u32;
                    let mut state = NotificationApp::new(
                        &self.conn,
                        &self.qh,
                        &self.compositor_state,
                        &self.xdg_shell_state,
                        pw,
                        ph,
                        scale,
                        &self.wgpu_instance,
                        &self.wgpu_adapter,
                        self.wgpu_device.clone(),
                        self.wgpu_queue.clone(),
                        &self.renderer_resources.cache,
                        opacity,
                    );
                    state.app_name = app_name;
                    state.summary = summary;
                    state.body = body;
                    state.needs_rebuild = true;
                    self.state = Some(state);
                } else if let Some(ref mut state) = self.state {
                    println!("[cce-notification-daemon] Updating active notification window: {} - {}", summary, body);
                    state.app_name = app_name;
                    state.summary = summary;
                    state.body = body;
                    state.opacity = opacity;
                    state.needs_rebuild = true;
                }
                self.redraw = true;

                // Schedule closing the window using the configured duration
                let duration_secs = read_duration_if_configured();
                let sender_clone = self.sender.clone();
                self.rt_handle.spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(duration_secs)).await;
                    let _ = sender_clone.send(UserEvent::CloseNotification { notification_id: active_id });
                });
            }
            UserEvent::CloseNotification { notification_id } => {
                if notification_id == self.current_id {
                    println!("[cce-notification-daemon] Closing notification window (ID: {})...", notification_id);
                    self.state = None; // Dropping the window and resources
                    self.redraw = true;
                }
            }
        }
    }
}

// ── D-Bus zbus implementation ──

struct DbusInterface {
    sender: calloop::channel::Sender<UserEvent>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl DbusInterface {
    async fn get_capabilities(&self) -> Vec<String> {
        vec![
            "body".to_string(),
            "actions".to_string(),
            "icon-static".to_string(),
        ]
    }

    async fn notify(
        &self,
        app_name: String,
        _replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, Value<'_>>,
        _expire_timeout: i32,
    ) -> u32 {
        let _ = self.sender.send(UserEvent::NewNotification {
            app_name,
            summary,
            body,
        });

        1 // zbus notification ID
    }

    async fn close_notification(&self, _id: u32) {}

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "cce-notification-daemon".to_string(),
            "CCEC Project".to_string(),
            "0.1.0".to_string(),
            "1.2".to_string(),
        )
    }
}

// ── main ──

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh).unwrap();
    let xdg_shell_state = XdgShell::bind(&globals, &qh).unwrap();
    let shm_state = Shm::bind(&globals, &qh).unwrap();
    let seat_state = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);

    // Initialize wgpu graphics context once on startup
    let wgpu_instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    });
    let wgpu_adapter = pollster::block_on(wgpu_instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
    })).expect("Failed to find wgpu adapter");
    let (wgpu_device, wgpu_queue) = pollster::block_on(wgpu_adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("GPU Device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(wgpu_adapter.limits()),
        memory_hints: wgpu::MemoryHints::MemoryUsage,
    }, None)).expect("Failed to request wgpu device");

    // Initialize renderer resources once
    let font_system = FontSystem::new();
    let swash_cache = SwashCache::new();
    let cache = Cache::new(&wgpu_device);
    let mut text_atlas = TextAtlas::new(&wgpu_device, &wgpu_queue, &cache, wgpu::TextureFormat::Bgra8Unorm);
    let text_renderer = TextRenderer::new(&mut text_atlas, &wgpu_device, wgpu::MultisampleState::default(), None);

    let shader_code = clear_ui::SHADER;
    let shader = wgpu_device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Shader"),
        source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(shader_code)),
    });
    let pipeline_layout = wgpu_device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Pipeline Layout"),
        bind_group_layouts: &[],
        push_constant_ranges: &[],
    });
    let render_pipeline = wgpu_device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Render Pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[Vertex::desc()],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Bgra8Unorm,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
            strip_index_format: None,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState { count: 1, mask: !0, alpha_to_coverage_enabled: false },
        multiview: None,
        cache: None,
    });

    let renderer_resources = RendererResources {
        render_pipeline,
        font_system,
        swash_cache,
        text_atlas,
        text_renderer,
        cache,
    };

    // Create the tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();

    let mut event_loop = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();

    let (sender, channel) = calloop::channel::channel::<UserEvent>();

    // Start D-Bus listener inside a background tokio thread pool
    let sender_clone = sender.clone();
    std::thread::spawn(move || {
        rt.block_on(async {
            let dbus_impl = DbusInterface { sender: sender_clone };
            let _connection = connection::Builder::session()
                .expect("Failed to connect to session bus")
                .name("org.freedesktop.Notifications")
                .expect("Failed to claim name org.freedesktop.Notifications")
                .serve_at("/org/freedesktop/Notifications", dbus_impl)
                .expect("Failed to register D-Bus path")
                .build()
                .await
                .expect("Failed to build D-Bus connection");

            println!("[cce-notification-daemon] D-Bus listener registered. Running...");
            
            // Keep background runtime alive
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        });
    });

    let mut app = AppState {
        registry_state: RegistryState::new(&globals),
        compositor_state,
        xdg_shell_state,
        shm_state,
        seat_state,
        output_state,
        seats: Vec::new(),
        pointer: None,
        keyboard: None,
        state: None,
        current_id: 0,
        exit: false,
        redraw: true,
        conn: conn.clone(),
        qh,
        rt_handle: handle,
        sender,
        wgpu_instance,
        wgpu_adapter,
        wgpu_device: wgpu_device.clone(),
        wgpu_queue: wgpu_queue.clone(),
        renderer_resources,
    };

    WaylandSource::new(conn, event_queue).insert(loop_handle.clone()).unwrap();

    loop_handle.insert_source(channel, |event, _metadata, app_state: &mut AppState| {
        if let calloop::channel::Event::Msg(msg) = event {
            app_state.handle_user_event(msg);
        }
    }).unwrap();

    println!("[cce-notification-daemon] Wayland event loop starting...");
    loop {
        if let Err(err) = event_loop.dispatch(std::time::Duration::from_millis(16), &mut app) {
            eprintln!("[cce-notification-daemon] Event loop error (exiting): {:?}", err);
            break;
        }
        if app.exit {
            break;
        }
        if app.redraw {
            app.redraw = false;
            if let Some(state) = &mut app.state {
                if state.configured {
                    let rr = &mut app.renderer_resources;
                    state.render(
                        &rr.render_pipeline,
                        &mut rr.font_system,
                        &mut rr.swash_cache,
                        &mut rr.text_atlas,
                        &mut rr.text_renderer,
                    );
                }
            }
        }
    }

    Ok(())
}
