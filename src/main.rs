use std::collections::HashMap;

use zbus::zvariant::Value;
use zbus::{connection, interface};

use wayland_client::QueueHandle;

use cce_ui::engine::{
    Application, EngineState, LayerAnchor, LayerKeyboardInteractivity, LayerKind, LayerSettings,
    LogicalPosition, WindowSettings,
};
use cce_ui::widget::{ElementState, KeyEvent, MouseButton, MouseScrollDelta};

const NOTIF_WIDTH: u32 = 360;
const NOTIF_HEIGHT: u32 = 100;

// Image previews (the freedesktop `image-path` hint, e.g. screenshots) fit
// this box, aspect-preserved, left of the text.
const THUMB_MAX_W: f32 = 100.0;
const THUMB_MAX_H: f32 = 76.0;

#[derive(Debug, Clone)]
enum UserEvent {
    NewNotification {
        app_name: String,
        summary: String,
        body: String,
        image_path: Option<String>,
    },
    CloseNotification,
}

// ── Config accessors ──────────────────────────────────────────────────────

fn read_duration() -> f32 {
    cce_ui::config::get_i64("/notifications/duration", 5) as f32
}

fn read_opacity() -> f32 {
    cce_ui::config::get_f32("/notifications/opacity", 0.9)
}

fn read_bg_color() -> [f32; 4] {
    cce_ui::config::get_string("/notifications/bg_color")
        .as_deref()
        .and_then(cce_ui::color::parse_hex_rgba_linear)
        .map(|[r, g, b, _]| [r, g, b, 1.0])
        .unwrap_or([
            cce_ui::colors::srgb_to_linear(0.08),
            cce_ui::colors::srgb_to_linear(0.08),
            cce_ui::colors::srgb_to_linear(0.12),
            1.0,
        ])
}

fn play_bell_if_configured() {
    let sound_type = cce_ui::config::get_string("/notifications/bell").unwrap_or_default();
    let sound_event = match sound_type.as_str() {
        "bell" => Some("bell"),
        "dialog" => Some("dialog-information"),
        "message" => Some("message"),
        _ => None,
    };
    if let Some(event) = sound_event {
        log::info!("Playing notification sound ({})...", sound_type);
        let _ = std::process::Command::new("canberra-gtk-play")
            .arg("-i")
            .arg(event)
            .spawn();
    }
}

/// Decode a PNG and nearest-neighbor downscale it to fit the thumbnail box.
/// Returns RGBA8 pixels plus dimensions; None for unreadable/non-PNG files.
fn load_thumbnail(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    let file = std::fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..w * h * 4].to_vec(),
        png::ColorType::Rgb => buf[..w * h * 3]
            .chunks_exact(3)
            .flat_map(|px| [px[0], px[1], px[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf[..w * h].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf[..w * h * 2]
            .chunks_exact(2)
            .flat_map(|px| [px[0], px[0], px[0], px[1]])
            .collect(),
        _ => return None,
    };

    let scale = (THUMB_MAX_W / w as f32).min(THUMB_MAX_H / h as f32).min(1.0);
    let (tw, th) = (
        ((w as f32 * scale) as usize).max(1),
        ((h as f32 * scale) as usize).max(1),
    );
    let mut thumb = Vec::with_capacity(tw * th * 4);
    for ty in 0..th {
        let sy = ty * h / th;
        for tx in 0..tw {
            let sx = tx * w / tw;
            let i = (sy * w + sx) * 4;
            thumb.extend_from_slice(&rgba[i..i + 4]);
        }
    }
    Some((thumb, tw as u32, th as u32))
}

fn srgb_u8(linear: [f32; 4]) -> [u8; 3] {
    let srgb = cce_ui::colors::to_srgb(linear);
    [
        (srgb[0] * 255.0) as u8,
        (srgb[1] * 255.0) as u8,
        (srgb[2] * 255.0) as u8,
    ]
}

// ── Application ───────────────────────────────────────────────────────────
//
// Phase 6 shape: the whole frame — accent quad and text — is one display list
// (`display_list` + `display_list_text`); the engine shapes the text through the shared
// buffer cache. No app-side FontSystem, TextItem cache, or rebuild bookkeeping.

struct NotifierApp {
    app_name: String,
    summary: String,
    body: String,
    /// Uploaded preview image (id from `vk::upload_rgba`, logical w, h).
    image: Option<(u32, f32, f32)>,
    visible: bool,
    dismiss_timer: f32,
    opacity: f32,
    bg_color: [f32; 4],
    sender: calloop::channel::Sender<UserEvent>,
}

impl NotifierApp {
    fn free_image(&mut self) {
        if let Some((id, _, _)) = self.image.take() {
            cce_ui::vk::free_image(id);
        }
    }
}

impl Application for NotifierApp {
    type Message = UserEvent;

    fn new(_qh: &QueueHandle<EngineState<Self>>, sender: calloop::channel::Sender<Self::Message>) -> Self {
        Self {
            app_name: String::new(),
            summary: String::new(),
            body: String::new(),
            image: None,
            visible: false,
            dismiss_timer: 0.0,
            opacity: read_opacity(),
            bg_color: read_bg_color(),
            sender,
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "cce-notifier".to_string(),
            app_id: "cce-notifier".to_string(),
            width: NOTIF_WIDTH,
            height: NOTIF_HEIGHT,
            fullscreen: false,
            min_size: None,
        }
    }

    fn layer(&self) -> Option<LayerSettings> {
        Some(LayerSettings {
            layer: LayerKind::Overlay,
            anchor: LayerAnchor::TOP | LayerAnchor::RIGHT,
            exclusive_zone: 0,
            keyboard_interactivity: LayerKeyboardInteractivity::None,
            margin: (20, 20, 0, 0),
            namespace: "cce-notifier".to_string(),
        })
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            UserEvent::NewNotification { app_name, summary, body, image_path } => {
                play_bell_if_configured();
                self.app_name = app_name;
                self.summary = summary;
                self.body = body;
                self.free_image();
                if let Some(path) = image_path {
                    if let Some((pixels, w, h)) = load_thumbnail(&path) {
                        let id = cce_ui::vk::upload_rgba(pixels, w, h);
                        self.image = Some((id, w as f32, h as f32));
                    }
                }
                self.opacity = read_opacity();
                self.bg_color = read_bg_color();
                self.visible = true;
                self.dismiss_timer = read_duration();
                *needs_rebuild = true;
            }
            UserEvent::CloseNotification => {
                self.visible = false;
                self.free_image();
                *needs_rebuild = true;
            }
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.visible {
            self.dismiss_timer -= dt;
            if self.dismiss_timer <= 0.0 {
                self.visible = false;
                self.free_image();
                *needs_rebuild = true;
            }
        }
    }

    /// The whole frame as one display list (Phase 6): the green accent border plus the three
    /// text lines. Coordinates are logical px; the engine applies HiDPI scale and shapes the
    /// text through its shared buffer cache.
    fn display_list(&mut self, _size: cce_ui::engine::LogicalSize, _scale: f64) -> Option<cce_ui::scene::paint::DisplayList> {
        use cce_ui::scene::layout::Rect;
        use cce_ui::scene::paint::PaintCtx;
        let mut pc = PaintCtx::new();
        if self.visible {
            pc.quad(
                Rect { x: 0.0, y: 0.0, width: 6.0, height: NOTIF_HEIGHT as f32 },
                cce_ui::colors::TOGGLE_ON,
            );
            // Preview thumbnail (screenshots etc.) centered in its box left of
            // the text, which shifts right to make room.
            let mut text_x = 18.0;
            if let Some((id, w, h)) = self.image {
                let ix = 14.0 + (THUMB_MAX_W - w) / 2.0;
                let iy = (NOTIF_HEIGHT as f32 - h) / 2.0;
                pc.image(id, Rect { x: ix, y: iy, width: w, height: h }, 1.0);
                text_x = 14.0 + THUMB_MAX_W + 12.0;
            }
            // Use a configured (bundled) font family so glyph font-ids resolve in the
            // engine's render FontSystem — a bare default can pick a system font absent
            // from the engine's bundled-only database.
            let family = cce_ui::layout::statusbar_font_parsed().0;
            let font = Some(family);
            pc.text_with(&self.app_name, text_x, 12.0, 10.0, srgb_u8(cce_ui::colors::TEXT_DIM), font.clone(), None);
            pc.text_with(&self.summary, text_x, 28.0, 13.0, srgb_u8(cce_ui::colors::TEXT_HEADER), font.clone(), None);
            pc.text_with(&self.body, text_x, 48.0, 11.0, srgb_u8(cce_ui::colors::TEXT_FG), font, None);
        }
        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn clear_color(&self) -> [f32; 4] {
        if self.visible {
            [self.bg_color[0], self.bg_color[1], self.bg_color[2], self.opacity]
        } else {
            [0.0, 0.0, 0.0, 0.0]
        }
    }

    /// When hidden, drop the input region so the transparent overlay is
    /// click-through; when visible, take input over the notification area.
    fn input_regions(&self) -> Option<Vec<(i32, i32, i32, i32)>> {
        if self.visible {
            None
        } else {
            Some(Vec::new())
        }
    }

    fn register_sources(&mut self, _handle: &calloop::LoopHandle<'_, EngineState<Self>>) {
        // Run the org.freedesktop.Notifications D-Bus server on a background
        // thread; incoming Notify calls are forwarded to update() via the channel.
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("cce-notifier: failed to start tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let dbus_impl = DbusInterface { sender };
                match connection::Builder::session()
                    .and_then(|b| b.name("org.freedesktop.Notifications"))
                    .and_then(|b| b.serve_at("/org/freedesktop/Notifications", dbus_impl))
                {
                    Ok(builder) => match builder.build().await {
                        Ok(_conn) => {
                            log::info!("cce-notifier: D-Bus listener registered.");
                            std::future::pending::<()>().await;
                        }
                        Err(e) => log::error!("cce-notifier: failed to build D-Bus connection: {e}"),
                    },
                    Err(e) => log::error!("cce-notifier: failed to register D-Bus name/path: {e}"),
                }
            });
        });
    }

    // Notifications are non-interactive.
    fn handle_pointer_move(&mut self, _pos: LogicalPosition, _needs_rebuild: &mut bool) {}
    fn handle_mouse_input(
        &mut self,
        _button: MouseButton,
        _state: ElementState,
        _pos: LogicalPosition,
        _needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        None
    }
    fn handle_mouse_wheel(&mut self, _delta: &MouseScrollDelta, _pos: LogicalPosition, _needs_rebuild: &mut bool) {}
    fn handle_key_input(&mut self, _event: &KeyEvent, _needs_rebuild: &mut bool) -> Option<Self::Message> {
        None
    }
}

// ── D-Bus (org.freedesktop.Notifications) ─────────────────────────────────

struct DbusInterface {
    sender: calloop::channel::Sender<UserEvent>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl DbusInterface {
    async fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_string(), "actions".to_string(), "icon-static".to_string()]
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        _replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        _expire_timeout: i32,
    ) -> u32 {
        // Preview image: the standard `image-path` hint (spec 1.2; `image_path`
        // is the 1.1 spelling), else an absolute-path app_icon.
        let hint_str = |key: &str| -> Option<String> {
            match hints.get(key) {
                Some(Value::Str(s)) => Some(s.to_string()),
                _ => None,
            }
        };
        let image_path = hint_str("image-path")
            .or_else(|| hint_str("image_path"))
            .or_else(|| app_icon.starts_with('/').then(|| app_icon.clone()));
        let _ = self.sender.send(UserEvent::NewNotification { app_name, summary, body, image_path });
        1
    }

    async fn close_notification(&self, _id: u32) {
        let _ = self.sender.send(UserEvent::CloseNotification);
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "cce-notifier".to_string(),
            "CCEC Project".to_string(),
            "0.1.0".to_string(),
            "1.2".to_string(),
        )
    }
}

fn main() {
    env_logger::init();
    cce_ui::engine::run::<NotifierApp>();
}
