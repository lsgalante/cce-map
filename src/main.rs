//! cce-map — slippy-map raster tile viewer (OpenStreetMap by default).
//!
//! View state is a Web-Mercator world coordinate (u, v) ∈ [0,1]² at the
//! window center plus a continuous zoom. Tiles render at the nearest
//! integer zoom, scaled to the continuous zoom; while a tile loads, the
//! nearest resident ancestor is drawn clipped to the tile's rect.

mod tiles;

use wayland_client::QueueHandle;

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::scroll_motion::{current_scroll_phase, scroll_settings, ScrollPhase};
use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};

use tiles::{TileKey, TileManager, MAX_ZOOM, TILE_SIZE};

const WHEEL_ZOOM_STEP: f64 = 0.25;
const KEY_PAN_PX: f64 = 120.0;

#[derive(Debug, Clone)]
enum Message {
    Tile { generation: u64, key: TileKey, image: Option<u32> },
}

struct MapApp {
    tiles: TileManager,
    /// Whether a renderer has been handed over yet — the first one is the
    /// process's own, any later one is a replacement after a reconnect. See
    /// `renderer_init`.
    seen_renderer: bool,
    /// World coords of the window center, u east [0,1), v south [0,1].
    center: (f64, f64),
    zoom: f64,
    /// Where the wheel is taking the zoom: each notch moves this and `tick`
    /// eases `zoom` toward it around `zoom_anchor` at cce-ui's wheel-glide
    /// rate (`scroll_ease`; instant with `smooth_scroll false`), so a burst
    /// of notches is one glide rather than a staircase. A trackpad, pinch,
    /// keys and Home set the zoom directly and pull the target along.
    zoom_target: f64,
    zoom_anchor: (f64, f64),
    win: (f32, f32),
    pointer: (f64, f64),
    drag: Option<(f64, f64)>,
}

/// Width of the whole world in logical pixels at a given zoom.
fn world_px(zoom: f64) -> f64 {
    TILE_SIZE * 2f64.powf(zoom)
}

impl MapApp {
    /// Change the zoom by `dz` keeping the world point under (px, py) fixed.
    fn zoom_step(&mut self, dz: f64, px: f64, py: f64) {
        let old = world_px(self.zoom);
        let new_zoom = (self.zoom + dz).clamp(0.0, MAX_ZOOM as f64);
        let new = world_px(new_zoom);
        let (w, h) = (self.win.0 as f64, self.win.1 as f64);
        let u = self.center.0 + (px - w / 2.0) / old;
        let v = self.center.1 + (py - h / 2.0) / old;
        self.center.0 = (u - (px - w / 2.0) / new).rem_euclid(1.0);
        self.center.1 = (v - (py - h / 2.0) / new).clamp(0.0, 1.0);
        self.zoom = new_zoom;
    }

    /// A direct zoom change (pinch, keys, trackpad): lands at once and
    /// cancels any wheel glide in flight.
    fn zoom_by(&mut self, dz: f64, px: f64, py: f64) {
        self.zoom_step(dz, px, py);
        self.zoom_target = self.zoom;
    }

    /// A wheel notch: retarget the glide around the pointer.
    fn zoom_wheel(&mut self, dz: f64, px: f64, py: f64) {
        self.zoom_anchor = (px, py);
        self.zoom_target = (self.zoom_target + dz).clamp(0.0, MAX_ZOOM as f64);
        if !scroll_settings().smooth {
            let remaining = self.zoom_target - self.zoom;
            self.zoom_step(remaining, px, py);
        }
    }

    /// Ease the zoom toward its wheel target; true while it moved.
    fn tick_zoom(&mut self, dt: f32) -> bool {
        let remaining = self.zoom_target - self.zoom;
        if remaining == 0.0 {
            return false;
        }
        let (ax, ay) = self.zoom_anchor;
        // Frame-rate independent exponential approach (cce-ui's glide),
        // snapping the last sliver so it settles instead of trailing off.
        let step = if remaining.abs() < 1e-3 {
            remaining
        } else {
            remaining * (1.0 - (-(scroll_settings().ease_rate as f64) * dt as f64).exp())
        };
        self.zoom_step(step, ax, ay);
        true
    }

    fn pan_px(&mut self, dx: f64, dy: f64) {
        let scale = world_px(self.zoom);
        self.center.0 = (self.center.0 + dx / scale).rem_euclid(1.0);
        self.center.1 = (self.center.1 + dy / scale).clamp(0.0, 1.0);
    }

    fn center_lat_lon(&self) -> (f64, f64) {
        let lon = self.center.0 * 360.0 - 180.0;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * self.center.1)).sinh().atan().to_degrees();
        (lat, lon)
    }
}

impl Application for MapApp {
    type Message = Message;

    fn new(_qh: &QueueHandle<EngineState<Self>>, sender: calloop::channel::Sender<Self::Message>) -> Self {
        Self {
            tiles: TileManager::new(sender),
            seen_renderer: false,
            center: (0.5, 0.5),
            zoom: 2.0,
            zoom_target: 2.0,
            zoom_anchor: (500.0, 350.0),
            win: (1000.0, 700.0),
            pointer: (0.0, 0.0),
            drag: None,
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Map".to_string(),
            app_id: "cce-map".to_string(),
            width: 1000,
            height: 700,
            fullscreen: false,
            min_size: Some((320, 240)),
        }
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            Message::Tile { generation, key, image } => {
                self.tiles.complete(generation, key, image);
                *needs_rebuild = true;
            }
        }
    }

    /// Re-fetch the visible tiles when the renderer is replaced.
    ///
    /// The tile store caches **renderer** image ids, which do not survive the
    /// reconnect `window_runner` performs around a live `Application` — see
    /// [`TileManager::reset`] for the whole story. Every resident tile is
    /// dropped here and the next paint asks for what it needs again, off the
    /// disk cache.
    ///
    /// Not on the first renderer: the tiles queued from `new()` are waiting
    /// for exactly that one.
    fn renderer_init(&mut self, _renderer: &mut cce_ui::vk::VkRenderer) {
        if std::mem::replace(&mut self.seen_renderer, true) {
            log::info!("[map] renderer replaced; re-fetching the resident tiles");
            self.tiles.reset();
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.tick_zoom(dt) {
            *needs_rebuild = true;
        }
    }

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        self.win = (width, height);
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let (px, py) = (pos.x as f64, pos.y as f64);
        if let Some((lx, ly)) = self.drag {
            self.pan_px(lx - px, ly - py);
            self.drag = Some((px, py));
            *needs_rebuild = true;
        }
        self.pointer = (px, py);
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        _needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        if button == MouseButton::Left {
            self.drag = match state {
                ElementState::Pressed => Some((pos.x as f64, pos.y as f64)),
                ElementState::Released => None,
            };
        }
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let notches = delta.notches_y() as f64;
        if notches == 0.0 {
            return;
        }
        let dz = notches * WHEEL_ZOOM_STEP;
        let (px, py) = (pos.x as f64, pos.y as f64);
        // A finger on a trackpad is followed 1:1 (nothing is smoother than
        // the hand); discrete notches glide.
        let finger = matches!(delta, MouseScrollDelta::PixelDelta(_))
            && matches!(current_scroll_phase(), ScrollPhase::Finger | ScrollPhase::FingerEnd);
        if finger {
            self.zoom_by(dz, px, py);
        } else {
            self.zoom_wheel(dz, px, py);
        }
        *needs_rebuild = true;
    }

    fn handle_pinch(&mut self, factor: f32, pos: LogicalPosition, needs_rebuild: &mut bool) -> bool {
        if factor > 0.0 && factor != 1.0 {
            self.zoom_by((factor as f64).log2(), pos.x as f64, pos.y as f64);
            *needs_rebuild = true;
        }
        true
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        if event.state != ElementState::Pressed {
            return None;
        }
        let (cx, cy) = (self.win.0 as f64 / 2.0, self.win.1 as f64 / 2.0);
        let mut handled = true;
        match &event.logical_key {
            Key::Character(c) if c == "+" || c == "=" => self.zoom_by(0.5, cx, cy),
            Key::Character(c) if c == "-" => self.zoom_by(-0.5, cx, cy),
            Key::Named(NamedKey::ArrowLeft) => self.pan_px(-KEY_PAN_PX, 0.0),
            Key::Named(NamedKey::ArrowRight) => self.pan_px(KEY_PAN_PX, 0.0),
            Key::Named(NamedKey::ArrowUp) => self.pan_px(0.0, -KEY_PAN_PX),
            Key::Named(NamedKey::ArrowDown) => self.pan_px(0.0, KEY_PAN_PX),
            Key::Named(NamedKey::Home) => {
                self.center = (0.5, 0.5);
                self.zoom = 2.0;
                self.zoom_target = 2.0;
            }
            _ => handled = false,
        }
        if handled {
            *needs_rebuild = true;
        }
        None
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<DisplayList> {
        self.win = (size.width, size.height);
        self.tiles.begin_frame();
        let mut pc = PaintCtx::new();
        let (w, h) = (size.width as f64, size.height as f64);
        pc.quad(
            Rect { x: 0.0, y: 0.0, width: size.width, height: size.height },
            [0.07, 0.08, 0.09, 1.0],
        );

        let scale_px = world_px(self.zoom);
        // Pick the tile zoom for physical resolution: on a scale-2 output a
        // z+1 tile drawn at 128 logical px is 1:1 physical. Backed off when
        // the viewport would need more resident tiles than the GPU image
        // registry (256, shared) comfortably holds.
        let mut tz = ((self.zoom + scale.max(1.0).log2()).round() as i32).clamp(0, MAX_ZOOM as i32) as u8;
        while tz > 0 {
            let tile_px = scale_px / (1u64 << tz) as f64;
            if (w / tile_px + 2.0) * (h / tile_px + 2.0) <= 160.0 {
                break;
            }
            tz -= 1;
        }
        let n = 1u64 << tz;
        let tile_px = scale_px / n as f64;
        // World coord of the window's top-left corner.
        let u0 = self.center.0 - w / 2.0 / scale_px;
        let v0 = self.center.1 - h / 2.0 / scale_px;
        // Unwrapped tile-index range covering the window (x wraps around
        // the antimeridian via rem_euclid; y is clamped to the world).
        let tx0 = (u0 * n as f64).floor() as i64;
        let tx1 = ((u0 + w / scale_px) * n as f64).floor() as i64;
        let ty0 = ((v0 * n as f64).floor() as i64).max(0);
        let ty1 = (((v0 + h / scale_px) * n as f64).floor() as i64).min(n as i64 - 1);

        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let key = TileKey {
                    z: tz,
                    x: tx.rem_euclid(n as i64) as u32,
                    y: ty as u32,
                };
                let rect = Rect {
                    x: ((tx as f64 / n as f64 - u0) * scale_px) as f32,
                    y: ((ty as f64 / n as f64 - v0) * scale_px) as f32,
                    width: tile_px as f32,
                    height: tile_px as f32,
                };
                if let Some(img) = self.tiles.ensure(key) {
                    pc.image(img, rect, 1.0);
                    continue;
                }
                // Loading: checkerboard placeholder, overdrawn by the
                // nearest resident ancestor scaled up and clipped.
                let shade = if (tx + ty) % 2 == 0 { 0.10 } else { 0.12 };
                pc.quad(rect, [shade, shade, shade + 0.01, 1.0]);
                for d in 1..=5u8 {
                    if d > tz {
                        break;
                    }
                    let az = tz - d;
                    let f = 1i64 << d;
                    let atx = tx.div_euclid(f);
                    let aty = ty.div_euclid(f);
                    let akey = TileKey {
                        z: az,
                        x: atx.rem_euclid((n / (f as u64)) as i64) as u32,
                        y: aty as u32,
                    };
                    if let Some(img) = self.tiles.ready(akey) {
                        let arect = Rect {
                            x: ((atx as f64 * f as f64 / n as f64 - u0) * scale_px) as f32,
                            y: ((aty as f64 * f as f64 / n as f64 - v0) * scale_px) as f32,
                            width: (tile_px * f as f64) as f32,
                            height: (tile_px * f as f64) as f32,
                        };
                        pc.clip(rect, |pc| pc.image(img, arect, 1.0));
                        break;
                    }
                }
            }
        }

        // HUD: zoom + center coordinates (top-left), attribution (bottom-right).
        let (lat, lon) = self.center_lat_lon();
        pc.quad(Rect { x: 8.0, y: 8.0, width: 232.0, height: 24.0 }, [0.0, 0.0, 0.0, 0.45]);
        pc.text(
            format!("z {:.2}   {:.4}°, {:.4}°", self.zoom, lat, lon),
            16.0,
            13.0,
            12.0,
            [230, 230, 230],
        );
        let attr_w = 200.0f32;
        pc.quad(
            Rect { x: size.width - attr_w, y: size.height - 24.0, width: attr_w, height: 24.0 },
            [0.0, 0.0, 0.0, 0.45],
        );
        pc.text(
            "© OpenStreetMap contributors",
            size.width - attr_w + 8.0,
            size.height - 19.0,
            11.0,
            [200, 200, 200],
        );

        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn clear_color(&self) -> [f32; 4] {
        [0.07, 0.08, 0.09, 1.0]
    }
}

fn main() {
    env_logger::init();
    cce_ui::engine::run::<MapApp>();
}
