//! Slippy-map tile store: disk cache + HTTP fetch workers + GPU-image LRU.
//!
//! Workers decode PNGs and call `cce_ui::vk::upload_rgba` directly (the
//! upload queue is thread-safe; the actual GPU work happens on the next
//! frame), then notify the app through the calloop channel so the engine
//! wakes and repaints.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crate::Message;

pub const TILE_SIZE: f64 = 256.0;
pub const MAX_ZOOM: u8 = 19;

/// GPU tiles kept resident. The cce-ui image registry hard-caps at 256
/// images total, so leave headroom for other consumers and churn.
const MAX_GPU_TILES: usize = 180;
const FETCH_THREADS: usize = 4;

/// OSM tile-usage policy requires an identifying User-Agent.
const USER_AGENT: &str = concat!("cce-map/", env!("CARGO_PKG_VERSION"), " (cce desktop environment)");
const DEFAULT_TILE_URL: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

enum TileState {
    Pending,
    Ready { image: u32, last_used: u64 },
    Failed,
}

pub struct TileManager {
    states: HashMap<TileKey, TileState>,
    queue: mpsc::Sender<TileKey>,
    /// Frame counter used as the LRU clock; bumped by the app each rebuild.
    frame: u64,
}

impl TileManager {
    pub fn new(notify: calloop::channel::Sender<Message>) -> Self {
        let (queue, rx) = mpsc::channel::<TileKey>();
        let rx = Arc::new(Mutex::new(rx));
        let url_template = std::env::var("CCE_MAP_TILE_URL").unwrap_or_else(|_| DEFAULT_TILE_URL.to_string());
        let cache_root = cache_root();
        for _ in 0..FETCH_THREADS {
            let rx = Arc::clone(&rx);
            let notify = notify.clone();
            let url_template = url_template.clone();
            let cache_root = cache_root.clone();
            std::thread::spawn(move || worker(rx, notify, url_template, cache_root));
        }
        Self { states: HashMap::new(), queue, frame: 0 }
    }

    pub fn begin_frame(&mut self) {
        self.frame += 1;
    }

    /// The tile's GPU image if resident (marks it used); otherwise queues a
    /// fetch (once) and returns None.
    pub fn ensure(&mut self, key: TileKey) -> Option<u32> {
        match self.states.get_mut(&key) {
            Some(TileState::Ready { image, last_used }) => {
                *last_used = self.frame;
                Some(*image)
            }
            Some(_) => None,
            None => {
                self.states.insert(key, TileState::Pending);
                let _ = self.queue.send(key);
                None
            }
        }
    }

    /// Like `ensure` but never queues a fetch — used for ancestor fallback.
    pub fn ready(&mut self, key: TileKey) -> Option<u32> {
        match self.states.get_mut(&key) {
            Some(TileState::Ready { image, last_used }) => {
                *last_used = self.frame;
                Some(*image)
            }
            _ => None,
        }
    }

    pub fn complete(&mut self, key: TileKey, image: Option<u32>) {
        let state = match image {
            Some(image) => TileState::Ready { image, last_used: self.frame },
            None => TileState::Failed,
        };
        self.states.insert(key, state);
        self.evict();
    }

    /// Free the least-recently-used GPU tiles once over budget. Tiles
    /// touched this frame are never evicted.
    fn evict(&mut self) {
        let resident = self.states.values().filter(|s| matches!(s, TileState::Ready { .. })).count();
        if resident <= MAX_GPU_TILES {
            return;
        }
        let mut ready: Vec<(TileKey, u64)> = self
            .states
            .iter()
            .filter_map(|(k, s)| match s {
                TileState::Ready { last_used, .. } if *last_used < self.frame => Some((*k, *last_used)),
                _ => None,
            })
            .collect();
        ready.sort_by_key(|&(_, used)| used);
        let excess = resident - MAX_GPU_TILES;
        for (key, _) in ready.into_iter().take(excess) {
            if let Some(TileState::Ready { image, .. }) = self.states.remove(&key) {
                cce_ui::vk::free_image(image);
            }
        }
    }
}

fn cache_root() -> PathBuf {
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"),
    };
    base.join("cce").join("map").join("tiles")
}

fn worker(
    rx: Arc<Mutex<mpsc::Receiver<TileKey>>>,
    notify: calloop::channel::Sender<Message>,
    url_template: String,
    cache_root: PathBuf,
) {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("http client");
    loop {
        let key = match rx.lock().unwrap().recv() {
            Ok(k) => k,
            Err(_) => return,
        };
        let image = fetch_tile(&client, &url_template, &cache_root, key)
            .map_err(|e| log::warn!("tile {}/{}/{}: {e}", key.z, key.x, key.y))
            .ok();
        if notify.send(Message::Tile { key, image }).is_err() {
            return;
        }
    }
}

fn fetch_tile(
    client: &reqwest::blocking::Client,
    url_template: &str,
    cache_root: &PathBuf,
    key: TileKey,
) -> Result<u32, String> {
    let path = cache_root.join(key.z.to_string()).join(key.x.to_string()).join(format!("{}.png", key.y));
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            let url = url_template
                .replace("{z}", &key.z.to_string())
                .replace("{x}", &key.x.to_string())
                .replace("{y}", &key.y.to_string());
            let resp = client.get(&url).send().map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("HTTP {}", resp.status()));
            }
            let bytes = resp.bytes().map_err(|e| e.to_string())?.to_vec();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, &bytes);
            bytes
        }
    };
    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(cce_ui::vk::upload_rgba(rgba.into_raw(), w, h))
}
