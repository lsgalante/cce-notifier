use std::collections::HashMap;

use zbus::zvariant::Value;
use zbus::{connection, interface};

use glyphon::FontSystem;
use wayland_client::QueueHandle;

use cce_ui::engine::{
    Application, EngineState, LayerAnchor, LayerKeyboardInteractivity, LayerKind, LayerSettings,
    LogicalPosition, LogicalSize, WindowSettings,
};
use cce_ui::widget::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, TextItem};

const NOTIF_WIDTH: u32 = 360;
const NOTIF_HEIGHT: u32 = 100;

#[derive(Debug, Clone)]
enum UserEvent {
    NewNotification {
        app_name: String,
        summary: String,
        body: String,
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

fn glyphon_color(linear: [f32; 4]) -> glyphon::Color {
    let srgb = cce_ui::colors::to_srgb(linear);
    glyphon::Color::rgb(
        (srgb[0] * 255.0) as u8,
        (srgb[1] * 255.0) as u8,
        (srgb[2] * 255.0) as u8,
    )
}

// ── Application ───────────────────────────────────────────────────────────

struct NotifierApp {
    font_system: FontSystem,
    app_name: String,
    summary: String,
    body: String,
    visible: bool,
    dismiss_timer: f32,
    opacity: f32,
    bg_color: [f32; 4],
    scale_factor: f64,
    needs_rebuild: bool,
    text_items: Vec<TextItem>,
    sender: calloop::channel::Sender<UserEvent>,
}

impl NotifierApp {
    fn rebuild_layout(&mut self) {
        self.text_items.clear();
        if self.visible {
            // app name, summary, body (logical px; the engine applies HiDPI scale)
            self.text_items.push(TextItem::new(
                &mut self.font_system,
                &self.app_name,
                10.0,
                18.0,
                12.0,
                glyphon_color(cce_ui::colors::TEXT_DIM),
                None,
                None,
            ));
            self.text_items.push(TextItem::new(
                &mut self.font_system,
                &self.summary,
                13.0,
                18.0,
                28.0,
                glyphon_color(cce_ui::colors::TEXT_HEADER),
                None,
                None,
            ));
            self.text_items.push(TextItem::new(
                &mut self.font_system,
                &self.body,
                11.0,
                18.0,
                48.0,
                glyphon_color(cce_ui::colors::TEXT_FG),
                None,
                None,
            ));
        }
        self.needs_rebuild = false;
    }
}

impl Application for NotifierApp {
    type Message = UserEvent;

    fn new(_qh: &QueueHandle<EngineState<Self>>, sender: calloop::channel::Sender<Self::Message>) -> Self {
        Self {
            font_system: cce_ui::create_font_system_with_system_fonts(),
            app_name: String::new(),
            summary: String::new(),
            body: String::new(),
            visible: false,
            dismiss_timer: 0.0,
            opacity: read_opacity(),
            bg_color: read_bg_color(),
            scale_factor: 1.0,
            needs_rebuild: false,
            text_items: Vec::new(),
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
            UserEvent::NewNotification { app_name, summary, body } => {
                play_bell_if_configured();
                self.app_name = app_name;
                self.summary = summary;
                self.body = body;
                self.opacity = read_opacity();
                self.bg_color = read_bg_color();
                self.visible = true;
                self.dismiss_timer = read_duration();
                self.needs_rebuild = true;
                *needs_rebuild = true;
            }
            UserEvent::CloseNotification => {
                self.visible = false;
                self.needs_rebuild = true;
                *needs_rebuild = true;
            }
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.visible {
            self.dismiss_timer -= dt;
            if self.dismiss_timer <= 0.0 {
                self.visible = false;
                self.needs_rebuild = true;
                *needs_rebuild = true;
            }
        }
    }

    fn view(&mut self, quads: &mut Vec<(f32, f32, f32, f32, [f32; 4])>, size: LogicalSize, scale: f64) {
        if self.needs_rebuild || (self.scale_factor - scale).abs() > f64::EPSILON {
            self.scale_factor = scale;
            self.rebuild_layout();
        }
        if self.visible {
            // Bright green left accent border.
            quads.push((0.0, 0.0, 6.0, size.height, cce_ui::colors::TOGGLE_ON));
        }
    }

    fn text_items(&self) -> &[TextItem] {
        &self.text_items
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
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, Value<'_>>,
        _expire_timeout: i32,
    ) -> u32 {
        let _ = self.sender.send(UserEvent::NewNotification { app_name, summary, body });
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
