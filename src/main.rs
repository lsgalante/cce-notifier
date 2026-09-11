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
const CARD_H: u32 = 100;
const CARD_GAP: u32 = 8;

/// How many notifications are on screen at once. Beyond this they queue: a
/// notification only starts its timer once it is displayed, so a burst is read
/// in order rather than scrolling past unseen.
const MAX_VISIBLE: usize = 5;

/// A layer surface's size is fixed at creation (the cce-ui engine has no
/// runtime resize for one), so the surface is always tall enough for a full
/// stack and the unused part is left transparent — and click-through, via the
/// per-card `input_regions` below.
const NOTIF_HEIGHT: u32 = MAX_VISIBLE as u32 * (CARD_H + CARD_GAP) - CARD_GAP;

// Image previews (the freedesktop `image-path` hint, e.g. screenshots) fit
// this box, aspect-preserved, left of the text.
const THUMB_MAX_W: f32 = 100.0;
const THUMB_MAX_H: f32 = 76.0;

// The text column's right and bottom padding, and where the body starts. The
// body gets whatever is left of the card below `BODY_TOP`.
const CARD_PAD: f32 = 14.0;
const CARD_PAD_B: f32 = 6.0;
const BODY_TOP: f32 = 46.0;
const BODY_SIZE: f32 = 11.0;

/// Body lines a card has room for: `box_height` (48) over the engine's 1.4 line
/// height at [`BODY_SIZE`] (15.4) — three lines, 46.2. Kept as a number because
/// the ellipsis fit has to reason about it, and asserted against the geometry
/// in the tests below so the two cannot drift.
const BODY_LINES: usize = 3;

/// Where the text column starts: the thumbnail pushes it right of the image box.
fn text_origin(has_image: bool) -> f32 {
    if has_image {
        14.0 + THUMB_MAX_W + 12.0
    } else {
        18.0
    }
}

/// The column the body wraps into — origin to the card's right padding.
fn body_wrap_width(has_image: bool) -> f32 {
    NOTIF_WIDTH as f32 - text_origin(has_image) - CARD_PAD
}

#[derive(Debug, Clone)]
enum UserEvent {
    NewNotification {
        /// Server-assigned, or the client's `replaces_id`: an arriving id that
        /// matches a live card updates it in place instead of stacking.
        id: u32,
        app_name: String,
        summary: String,
        body: String,
        image_path: Option<String>,
        /// Seconds, already resolved from the client's `expire_timeout`.
        duration: f32,
    },
    CloseNotification {
        id: u32,
    },
}

// ── Config accessors ──────────────────────────────────────────────────────

fn read_duration() -> f32 {
    cce_ui::config::get_i64("/notifications/duration", 5) as f32
}

/// The notification plate style: per-app `plate { }` keys from
/// `~/.config/cce/cce-notifier/config.kdl` (merged over the global config by
/// `parse_kdl_to_json`), falling back to the shared `style.surface.plate`
/// values for anything unset.
struct PlateStyle {
    fill: [f32; 4],
    border: Option<([f32; 4], f32)>,
    radius: f32,
    blur: bool,
    opacity: f32,
}

fn read_plate_style() -> PlateStyle {
    let cfg = cce_ui::config::cached_config();
    let key = |name: &str| cfg.pointer(&format!("/plate/{name}"));
    let f32_key = |name: &str, default: f32| {
        key(name).and_then(|v| v.as_f64()).map(|f| f as f32).unwrap_or(default)
    };
    // Shared plate colors are stored linear (color.rs gamma-corrects on load),
    // so the override keys parse linear too.
    let linear = |name: &str| {
        key(name)
            .and_then(|v| v.as_str())
            .and_then(cce_ui::color::parse_hex_rgba_linear)
    };
    let fill = linear("color")
        .or_else(cce_ui::colors::plate_color)
        .unwrap_or([
            cce_ui::colors::srgb_to_linear(0.08),
            cce_ui::colors::srgb_to_linear(0.08),
            cce_ui::colors::srgb_to_linear(0.12),
            1.0,
        ]);
    let border = linear("border_color")
        .or_else(cce_ui::colors::plate_border_color)
        .map(|c| (c, f32_key("border_thickness", cce_ui::colors::plate_border_thickness())))
        .filter(|&(_, t)| t > 0.0);
    PlateStyle {
        fill,
        border,
        radius: f32_key("corner_radius", cce_ui::layout::plate_corner_radius()),
        blur: key("blur").and_then(|v| v.as_bool()).unwrap_or_else(cce_ui::colors::plate_blur),
        opacity: f32_key("opacity", cce_ui::layout::plate_opacity()),
    }
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

/// How many lines `text` wraps to in a column `wrap_w` wide.
///
/// Measured through the engine's OWN boxed-text layout — the same call the
/// renderer makes for a `text_boxed` prim — so the count cannot drift from what
/// is actually drawn (its 1.4 line height and the scale factor are applied
/// inside). The height is left effectively unbounded on purpose: pass the card's
/// real `box_height` and cosmic-text stops shaping at the lines that fit, which
/// is exactly the overflow this needs to detect.
fn wrapped_line_count(text: &str, wrap_w: f32, font: Option<&str>) -> usize {
    let Ok(mut fs) = cce_ui::geometry_font_system().lock() else {
        return 1;
    };
    let (buffer, _) = cce_ui::engine::get_text_buffer_laid_out(
        &mut fs,
        text,
        BODY_SIZE,
        font,
        cce_ui::scene::paint::TextAttrs::default(),
        cce_ui::scene::paint::TextLayout {
            wrap_width: Some(wrap_w),
            box_height: 10_000.0,
            align_h: cce_ui::scene::paint::AlignH::Left,
            align_v: cce_ui::scene::paint::AlignV::Top,
        },
    );
    buffer.layout_runs().count()
}

/// The body as it should be drawn: unchanged when it fits, else cut to the most
/// text that still fits [`BODY_LINES`] lines *with* an ellipsis appended.
///
/// A card is a fixed height, so a long body is truncated either way — this only
/// decides whether the reader can see that it was. Binary search over the char
/// prefix, re-measuring each candidate, because where the text wraps (and so
/// how much fits) depends on the words themselves, not the character count.
/// Called once per notification on arrival, never per frame.
fn fit_body(body: &str, wrap_w: f32, font: Option<&str>) -> String {
    if body.is_empty() || wrapped_line_count(body, wrap_w, font) <= BODY_LINES {
        return body.to_string();
    }
    let chars: Vec<char> = body.chars().collect();
    let with_ellipsis = |n: usize| -> String {
        let mut s: String = chars[..n].iter().collect();
        // Trim first so the ellipsis follows the word, not the space after it.
        while s.ends_with(char::is_whitespace) {
            s.pop();
        }
        s.push('…');
        s
    };
    // Largest prefix that still fits. `fits(0)` is just the ellipsis, so the
    // search always has an answer to fall back on.
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if wrapped_line_count(&with_ellipsis(mid), wrap_w, font) <= BODY_LINES {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    with_ellipsis(lo)
}

fn srgb_u8(linear: [f32; 4]) -> [u8; 3] {
    let srgb = cce_ui::colors::to_srgb(linear);
    [
        (srgb[0] * 255.0) as u8,
        (srgb[1] * 255.0) as u8,
        (srgb[2] * 255.0) as u8,
    ]
}

/// Paint one notification card with its top edge at `top` in surface-local
/// logical px. Everything is offset from there, so a card's position in the
/// stack is the only thing that changes between slots.
fn draw_card(
    pc: &mut cce_ui::scene::paint::PaintCtx,
    plate: &PlateStyle,
    notification: &Notification,
    top: f32,
) {
    use cce_ui::scene::layout::Rect;
    let card_h = CARD_H as f32;
    // The plate (per-app `plate { }` keys over the shared plate style;
    // blur via the negative-alpha marker), replacing the old clear-color background.
    let surface = Rect { x: 0.0, y: top, width: NOTIF_WIDTH as f32, height: card_h };
    let radius = plate.radius;
    let mut fill = plate.fill;
    fill[3] *= plate.opacity;
    if plate.blur {
        fill[3] = -fill[3].abs();
    }
    match plate.border {
        Some((border, thickness)) => {
            pc.border(surface, (radius, radius, radius, radius), fill, border, thickness)
        }
        None => {
            let on = radius > 0.0;
            pc.rounded_rect(surface, radius, (on, on, on, on), fill);
        }
    }
    // Preview thumbnail (screenshots etc.) centered in its box left of
    // the text, which shifts right to make room.
    let text_x = text_origin(notification.image.is_some());
    if let Some((id, w, h)) = notification.image {
        let ix = 14.0 + (THUMB_MAX_W - w) / 2.0;
        let iy = top + (card_h - h) / 2.0;
        pc.image(id, Rect { x: ix, y: iy, width: w, height: h }, 1.0);
    }
    // Use a configured (bundled) font family so glyph font-ids resolve in the
    // engine's render FontSystem — a bare default can pick a system font absent
    // from the engine's bundled-only database.
    let family = cce_ui::layout::statusbar_font_parsed().0;
    let font = Some(family);
    // The text column: from `text_x` (which the thumbnail may have pushed right)
    // to the card's right padding. Every label is bounded by it, so nothing runs
    // out over the plate's edge and rounded corner.
    let text_w = body_wrap_width(notification.image.is_some());
    let column = |t: f32, b: f32| Some([text_x, top + t, text_x + text_w, top + b]);
    pc.text_with(&notification.app_name, text_x, top + 12.0, 10.0, srgb_u8(cce_ui::colors::TEXT_DIM), font.clone(), column(8.0, BODY_TOP));
    pc.text_with(&notification.summary, text_x, top + 28.0, 13.0, srgb_u8(cce_ui::colors::TEXT_HEADER), font.clone(), column(24.0, BODY_TOP));
    // The body word-wraps within that column instead of running off the card.
    // `box_height` is what bounds it: the engine lays boxed text out at a 1.4
    // line height, so this admits BODY_LINES 11px lines (46.2 of 48) and shapes
    // away the rest — a card is a fixed height and cannot grow to fit. Anything
    // longer was already cut to fit with an ellipsis by `fit_body` on arrival,
    // so this bound is a backstop, not the truncation.
    pc.text_boxed(
        &notification.body,
        text_x,
        top + BODY_TOP,
        BODY_SIZE,
        srgb_u8(cce_ui::colors::TEXT_FG),
        font,
        column(BODY_TOP, card_h - CARD_PAD_B),
        cce_ui::scene::paint::TextAttrs::default(),
        cce_ui::scene::paint::TextLayout {
            wrap_width: Some(text_w),
            box_height: card_h - BODY_TOP - CARD_PAD_B,
            align_h: cce_ui::scene::paint::AlignH::Left,
            align_v: cce_ui::scene::paint::AlignV::Top,
        },
    );
}

// ── Application ───────────────────────────────────────────────────────────
//
// Phase 6 shape: the whole frame — plate and text — is one display list
// (`display_list` + `display_list_text`); the engine shapes the text through the shared
// buffer cache. No app-side FontSystem, TextItem cache, or rebuild bookkeeping.

struct Notification {
    id: u32,
    app_name: String,
    summary: String,
    /// Already fitted to the card by [`fit_body`] on arrival — ellipsis and all.
    body: String,
    /// Uploaded preview image (id from `vk::upload_rgba`, logical w, h).
    image: Option<(u32, f32, f32)>,
    /// Seconds the card is asked to stay up once it is displayed.
    duration: f32,
    /// When the card comes down, set on the first tick it is displayed — so a
    /// queued notification does not expire before it is ever shown. A wall
    /// clock, deliberately, not an accumulation of the runner's `dt`: `dt` is
    /// animation time, clamped to one frame after an idle sleep, and a card
    /// whose only reason to redraw is its own expiry never leaves that sleep.
    /// Ageing it by `dt` ran the countdown at 1/60 speed — a 5 s toast sat on
    /// screen for five minutes.
    expires_at: Option<std::time::Instant>,
}

impl Notification {
    fn free_image(&mut self) {
        if let Some((id, _, _)) = self.image.take() {
            cce_ui::vk::free_image(id);
        }
    }

    fn top(index: usize) -> f32 {
        index as f32 * (CARD_H + CARD_GAP) as f32
    }
}

struct NotifierApp {
    /// Live notifications, oldest first. The first `MAX_VISIBLE` are drawn top-down
    /// (so a new one appears below the ones already being read, and cards below an
    /// expiring one slide up); the rest wait their turn.
    stack: Vec<Notification>,
    plate: PlateStyle,
    sender: calloop::channel::Sender<UserEvent>,
}

impl NotifierApp {
    fn visible_count(&self) -> usize {
        self.stack.len().min(MAX_VISIBLE)
    }
}

impl Application for NotifierApp {
    type Message = UserEvent;

    fn new(_qh: &QueueHandle<EngineState<Self>>, sender: calloop::channel::Sender<Self::Message>) -> Self {
        Self {
            stack: Vec::new(),
            plate: read_plate_style(),
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
        // Sit below the status modules with one bar-spacing of gap, right edge aligned
        // with the rightmost top-right module (the clock): the compositor lays the bar
        // out at y=0, height `layout.bar_height`, flush to `output_width - MARGIN`
        // with SPACING between segments (cce-window-manager arrange.rs, both 12).
        let bar_h = cce_ui::config::get_i64("/layout/bar_height", 24) as i32;
        Some(LayerSettings {
            layer: LayerKind::Overlay,
            anchor: LayerAnchor::TOP | LayerAnchor::RIGHT,
            exclusive_zone: 0,
            keyboard_interactivity: LayerKeyboardInteractivity::None,
            margin: (bar_h + 12, 12, 0, 0),
            namespace: "cce-notifier".to_string(),
        })
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            UserEvent::NewNotification { id, app_name, summary, body, image_path, duration } => {
                play_bell_if_configured();
                let image = image_path
                    .as_deref()
                    .and_then(load_thumbnail)
                    .map(|(pixels, w, h)| {
                        (cce_ui::vk::upload_rgba(pixels, w, h), w as f32, h as f32)
                    });
                // Fit the body to the card once, here, rather than per frame:
                // the column it wraps into depends on whether a thumbnail
                // pushed the text right, which is settled by now.
                let family = cce_ui::layout::statusbar_font_parsed().0;
                let body = fit_body(&body, body_wrap_width(image.is_some()), Some(&family));
                let fresh = Notification { id, app_name, summary, body, image, duration, expires_at: None };
                // A repeat of a live id (volume steps, download progress) refreshes that
                // card where it sits rather than growing the stack.
                match self.stack.iter().position(|n| n.id == id) {
                    Some(i) => {
                        self.stack[i].free_image();
                        self.stack[i] = fresh;
                    }
                    None => self.stack.push(fresh),
                }
                self.plate = read_plate_style();
                *needs_rebuild = true;
            }
            UserEvent::CloseNotification { id } => {
                if let Some(i) = self.stack.iter().position(|n| n.id == id) {
                    self.stack.remove(i).free_image();
                    *needs_rebuild = true;
                }
            }
        }
    }

    fn tick(&mut self, _dt: f32, needs_rebuild: &mut bool) {
        // Only displayed cards age; queued ones keep their full duration and
        // start counting on the first tick after a slot frees up.
        let now = std::time::Instant::now();
        let visible = self.visible_count();
        for n in &mut self.stack[..visible] {
            let deadline = now + std::time::Duration::from_secs_f32(n.duration);
            n.expires_at.get_or_insert(deadline);
        }
        let before = self.stack.len();
        let mut i = 0;
        while i < self.stack.len() {
            if self.stack[i].expires_at.is_some_and(|t| now >= t) {
                self.stack.remove(i).free_image();
            } else {
                i += 1;
            }
        }
        if self.stack.len() != before {
            *needs_rebuild = true;
        }
    }

    /// A live card's only pending work is its own expiry, which the runner
    /// cannot see: nothing redraws, so the loop parks on the default idle
    /// sleep and the card outstays its welcome by up to a second. Poll while
    /// the stack is occupied, and go fully idle the moment it empties.
    fn idle_poll_interval(&self) -> Option<std::time::Duration> {
        (!self.stack.is_empty()).then(|| std::time::Duration::from_millis(100))
    }

    /// The whole frame as one display list (Phase 6): the plate plus the three
    /// text lines. Coordinates are logical px; the engine applies HiDPI scale and shapes the
    /// text through its shared buffer cache.
    fn display_list(&mut self, _size: cce_ui::engine::LogicalSize, _scale: f64) -> Option<cce_ui::scene::paint::DisplayList> {
        use cce_ui::scene::paint::PaintCtx;
        let mut pc = PaintCtx::new();
        for (i, notification) in self.stack.iter().take(MAX_VISIBLE).enumerate() {
            draw_card(&mut pc, &self.plate, notification, Notification::top(i));
        }
        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    /// Always transparent: the background is the plate drawn in `display_list`, so the
    /// surface itself stays clear (and rounded plate corners show through).
    fn clear_color(&self) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    /// One region per displayed card, so the gaps between them and the unused
    /// tail of the tall surface stay click-through. Always `Some`: the engine
    /// only touches the input region when this returns one, so a `None` here
    /// would leave the last region set — including the empty one that makes an
    /// idle stack transparent to clicks.
    fn input_regions(&self) -> Option<Vec<(i32, i32, i32, i32)>> {
        Some(
            (0..self.visible_count())
                .map(|i| (0, Notification::top(i) as i32, NOTIF_WIDTH as i32, CARD_H as i32))
                .collect(),
        )
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
                let dbus_impl = DbusInterface {
                    sender,
                    next_id: std::sync::atomic::AtomicU32::new(1),
                };
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
    /// Ids handed back to clients, so they can later replace or close a
    /// specific notification. Lives here, not in the app, because `Notify`
    /// has to return the id synchronously to its caller.
    next_id: std::sync::atomic::AtomicU32,
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
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        expire_timeout: i32,
    ) -> u32 {
        use std::sync::atomic::Ordering;
        // A client-chosen `replaces_id` is honored as-is (the spec requires the
        // same id back), so the counter is pushed past it to keep a later
        // server-assigned id from colliding with a card that is still live.
        let id = if replaces_id != 0 {
            self.next_id.fetch_max(replaces_id + 1, Ordering::Relaxed);
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        };
        // `expire_timeout` is ms; -1 means "server decides". 0 means "never
        // expire" in the spec, but these cards cannot be clicked away, so it
        // is treated as the default rather than pinning a slot forever.
        let duration = match expire_timeout {
            ms if ms > 0 => ms as f32 / 1000.0,
            _ => read_duration(),
        }
        .max(1.0);
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
        let _ = self.sender.send(UserEvent::NewNotification {
            id,
            app_name,
            summary,
            body,
            image_path,
            duration,
        });
        id
    }

    async fn close_notification(&self, id: u32) {
        let _ = self.sender.send(UserEvent::CloseNotification { id });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `BODY_LINES` restates the card's geometry, and `fit_body` trusts it to
    /// decide where the ellipsis goes. If the card height, the body's top, or
    /// its font size moves, this is what says so.
    #[test]
    fn body_lines_matches_the_card_geometry() {
        let box_height = CARD_H as f32 - BODY_TOP - CARD_PAD_B;
        let line_height = BODY_SIZE * 1.4; // the engine's boxed-text line height
        assert!(
            line_height * BODY_LINES as f32 <= box_height,
            "BODY_LINES={BODY_LINES} needs {} of {box_height}",
            line_height * BODY_LINES as f32
        );
        assert!(
            line_height * (BODY_LINES + 1) as f32 > box_height,
            "another line fits in {box_height} — BODY_LINES is too small"
        );
    }

    /// A full stack has to land exactly on the surface the layer shell was
    /// given, since that height is fixed at creation and cannot be renegotiated.
    #[test]
    fn a_full_stack_fits_the_surface() {
        assert_eq!(
            Notification::top(MAX_VISIBLE - 1) + CARD_H as f32,
            NOTIF_HEIGHT as f32
        );
    }
}
