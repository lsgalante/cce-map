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
use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};

use tiles::{TileKey, TileManager, MAX_ZOOM, TILE_SIZE};

const WHEEL_ZOOM_STEP: f64 = 0.25;
const KEY_PAN_PX: f64 = 120.0;

#[derive(Debug, Clone)]
enum Message {
    Tile { key: TileKey, image: Option<u32> },
}

struct MapApp {
    tiles: TileManager,
    /// World coords of the window center, u east [0,1), v south [0,1].
    center: (f64, f64),
    zoom: f64,
    win: (f32, f32),
    pointer: (f64, f64),
    drag: Option<(f64, f64)>,
}

/// Width of the whole world in logical pixels at a given zoom.
fn world_px(zoom: f64) -> f64 {
    TILE_SIZE * 2f64.powf(zoom)
}

impl MapApp {
    fn zoom_by(&mut self, dz: f64, px: f64, py: f64) {
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
            center: (0.5, 0.5),
            zoom: 2.0,
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
            Message::Tile { key, image } => {
                self.tiles.complete(key, image);
                *needs_rebuild = true;
            }
        }
    }

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

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
        if notches != 0.0 {
            self.zoom_by(notches * WHEEL_ZOOM_STEP, pos.x as f64, pos.y as f64);
            *needs_rebuild = true;
        }
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
            }
            _ => handled = false,
        }
        if handled {
            *needs_rebuild = true;
        }
        None
    }

    fn display_list(&mut self, size: LogicalSize, _scale: f64) -> Option<DisplayList> {
        self.win = (size.width, size.height);
        self.tiles.begin_frame();
        let mut pc = PaintCtx::new();
        let (w, h) = (size.width as f64, size.height as f64);
        pc.quad(
            Rect { x: 0.0, y: 0.0, width: size.width, height: size.height },
            [0.07, 0.08, 0.09, 1.0],
        );

        let scale_px = world_px(self.zoom);
        let tz = (self.zoom.round() as i32).clamp(0, MAX_ZOOM as i32) as u8;
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
