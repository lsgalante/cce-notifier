use std::collections::HashMap;
use std::sync::Arc;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes};
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;
use zbus::{interface, connection};
use zbus::zvariant::Value;
use glyphon::{
    Attrs, Buffer, Cache, FontSystem, Metrics, Resolution, SwashCache, TextArea, TextAtlas,
    TextBounds, TextRenderer, Viewport,
};

// ── Vertices and rendering structures ──

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
        0 => Float32x2,
        1 => Float32x4,
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
        Vertex { position: [x0, y0], color: c },
        Vertex { position: [x1, y0], color: c },
        Vertex { position: [x0, y1], color: c },
        Vertex { position: [x1, y0], color: c },
        Vertex { position: [x1, y1], color: c },
        Vertex { position: [x0, y1], color: c },
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

struct NotificationApp {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,

    font_system: FontSystem,
    swash_cache: SwashCache,
    text_atlas: TextAtlas,
    text_renderer: TextRenderer,
    text_viewport: Viewport,

    rects: Vec<RectWidget>,
    text_items: Vec<TextItem>,

    scale_factor: f64,
    width: u32,
    height: u32,
    needs_rebuild: bool,

    app_name: String,
    summary: String,
    body: String,
}

impl NotificationApp {
    async fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }).await.expect("adapter");
        let (device, queue) = adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("GPU Device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
        }, None).await.expect("device");
        let config = surface.get_default_config(&adapter, size.width.max(1), size.height.max(1)).expect("config");
        surface.configure(&device, &config);

        let shader_code = clear_ui::SHADER;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(shader_code)),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                    format: config.format,
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

        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let mut text_atlas = TextAtlas::new(&device, &queue, &cache, config.format);
        let text_renderer = TextRenderer::new(&mut text_atlas, &device, wgpu::MultisampleState::default(), None);
        let mut text_viewport = Viewport::new(&device, &cache);
        text_viewport.update(&queue, Resolution { width: size.width, height: size.height });

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Vertex Buffer"),
            size: 1,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let scale_factor = (window.scale_factor() as f32).max(2.0) as f64;

        Self {
            window,
            surface,
            device,
            queue,
            config,
            render_pipeline,
            vertex_buffer,
            vertex_count: 0,
            font_system,
            swash_cache,
            text_atlas,
            text_renderer,
            text_viewport,
            rects: Vec::new(),
            text_items: Vec::new(),
            scale_factor,
            width: size.width,
            height: size.height,
            needs_rebuild: true,
            app_name: String::new(),
            summary: String::new(),
            body: String::new(),
        }
    }

    fn rebuild_layout(&mut self) {
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
            color: clear_ui::colors::HEADER_BG,
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
        let app_name_buf = make_text_buffer(&mut self.font_system, &self.app_name, 10.0 * s);
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

        let summary_buf = make_text_buffer(&mut self.font_system, &self.summary, 13.0 * s);
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

        let body_buf = make_text_buffer(&mut self.font_system, &self.body, 11.0 * s);
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

    fn prepare_text(&mut self) {
        let w = self.width as f32;
        let h = self.height as f32;
        let viewport = Resolution { width: w as u32, height: h as u32 };
        self.text_viewport.update(&self.queue, viewport);
        let bounds = TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        let areas: Vec<TextArea> = self.text_items.iter().map(|ti| TextArea {
            buffer: &ti.buffer,
            left: ti.x, top: ti.y, scale: 1.0, bounds,
            default_color: ti.color,
            custom_glyphs: &[],
        }).collect();
        self.text_renderer.prepare(
            &self.device, &self.queue, &mut self.font_system,
            &mut self.text_atlas, &self.text_viewport, areas, &mut self.swash_cache
        ).unwrap();
    }

    fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        if size.width > 0 && size.height > 0 {
            self.width = size.width;
            self.height = size.height;
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
            self.needs_rebuild = true;
        }
    }

    fn render(&mut self) {
        if self.needs_rebuild {
            self.rebuild_layout();
            self.upload_vertices();
        }
        self.prepare_text();

        let output = match self.surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
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
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.08, g: 0.08, b: 0.12, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_pipeline(&self.render_pipeline);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.draw(0..self.vertex_count, 0..1);

            self.text_renderer.render(&self.text_atlas, &self.text_viewport, &mut pass).unwrap();
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.window.pre_present_notify();
        output.present();
    }
}

// ── D-Bus Events & AppWrapper ──

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

struct AppWrapper {
    proxy: EventLoopProxy<UserEvent>,
    rt_handle: tokio::runtime::Handle,
    state: Option<NotificationApp>,
    current_id: u32,
}

impl ApplicationHandler<UserEvent> for AppWrapper {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // We run in background, do not open windows until a notification event arrives
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::NewNotification { app_name, summary, body } => {
                self.current_id += 1;
                let active_id = self.current_id;

                if self.state.is_none() {
                    println!("[clear-notifier] Opening notification window: {} - {}", summary, body);
                    let mut attributes = WindowAttributes::default()
                        .with_title("Notification")
                        .with_decorations(false)
                        .with_inner_size(winit::dpi::LogicalSize::new(360, 100));
                    #[cfg(target_os = "linux")]
                    {
                        attributes = attributes.with_name("clear-notifier", "clear-notifier");
                    }
                    let window = Arc::new(event_loop.create_window(attributes).unwrap());
                    let mut state = pollster::block_on(NotificationApp::new(window));
                    state.app_name = app_name;
                    state.summary = summary;
                    state.body = body;
                    state.needs_rebuild = true;
                    self.state = Some(state);
                } else if let Some(ref mut state) = self.state {
                    println!("[clear-notifier] Updating active notification window: {} - {}", summary, body);
                    state.app_name = app_name;
                    state.summary = summary;
                    state.body = body;
                    state.needs_rebuild = true;
                    state.window.request_redraw();
                }

                // Schedule closing the window in 5 seconds
                let proxy_clone = self.proxy.clone();
                self.rt_handle.spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    let _ = proxy_clone.send_event(UserEvent::CloseNotification { notification_id: active_id });
                });
            }
            UserEvent::CloseNotification { notification_id } => {
                // Only close the window if no newer notification has taken over
                if notification_id == self.current_id {
                    println!("[clear-notifier] Closing notification window (ID: {})...", notification_id);
                    self.state = None; // Dropping the window and resources
                }
            }
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: winit::window::WindowId, event: WindowEvent) {
        if let Some(ref mut state) = self.state {
            match event {
                WindowEvent::CloseRequested => {
                    self.state = None;
                }
                WindowEvent::Resized(s) => {
                    state.resize(s);
                }
                WindowEvent::RedrawRequested => {
                    state.render();
                }
                _ => {}
            }
        }
    }
}

// ── D-Bus zbus implementation ──

struct DbusInterface {
    proxy: EventLoopProxy<UserEvent>,
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
        let _ = self.proxy.send_event(UserEvent::NewNotification {
            app_name,
            summary,
            body,
        });

        1 // zbus notification ID
    }

    async fn close_notification(&self, _id: u32) {}

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "clear-notifier".to_string(),
            "ClearWM Project".to_string(),
            "0.1.0".to_string(),
            "1.2".to_string(),
        )
    }
}

// ── main ──

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create the winit event loop
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // Create the tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();

    // Start D-Bus listener inside a background tokio thread pool
    std::thread::spawn(move || {
        rt.block_on(async {
            let dbus_impl = DbusInterface { proxy };
            let _connection = connection::Builder::session()
                .expect("Failed to connect to session bus")
                .name("org.freedesktop.Notifications")
                .expect("Failed to claim name org.freedesktop.Notifications")
                .serve_at("/org/freedesktop/Notifications", dbus_impl)
                .expect("Failed to register D-Bus path")
                .build()
                .await
                .expect("Failed to build D-Bus connection");

            println!("[clear-notifier] D-Bus listener registered. Running...");
            
            // Keep background runtime alive
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        });
    });

    let mut app = AppWrapper {
        proxy: event_loop.create_proxy(),
        rt_handle: handle,
        state: None,
        current_id: 0,
    };

    println!("[clear-notifier] winit event loop starting...");
    event_loop.run_app(&mut app)?;

    Ok(())
}
