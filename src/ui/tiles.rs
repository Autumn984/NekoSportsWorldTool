//! 地图瓦片层：高德道路瓦片（GCJ-02，免 key）+ 磁盘缓存 + egui 纹理 LRU。
//!
//! - 瓦片源：`webrd0{1-4}.is.autonavi.com/appmaptile?style=7`（高德公开道路瓦片，
//!   GCJ-02 基准，与境内地图显示一致，无需 key；仅作地图底图显示用途）。
//! - 下载线程：独立 std 线程 + 双通道（请求/完成），失败瓦片本次会话不再重试。
//! - 磁盘缓存：`<数据目录>/map_tiles/{z}/{x}/{y}.png`，重启后直接命中。
//! - 纹理：egui TextureHandle 按 LRU 保留约 256 张。

use eframe::egui;
use std::collections::{HashMap, HashSet};

pub const TILE_PX: f64 = 256.0;
pub const MAX_TILES: usize = 256;
pub const ZOOM_MIN: f32 = 3.0;
pub const ZOOM_MAX: f32 = 18.0;

/// 高德道路瓦片 URL（style=7 路网；子域 1-4 轮换）。
pub fn amap_tile_url(x: u32, y: u32, z: u32) -> String {
    let sub = (x + y) % 4 + 1;
    format!(
        "https://webrd0{sub}.is.autonavi.com/appmaptile?lang=zh_cn&size=1&scale=1&style=7&x={x}&y={y}&z={z}"
    )
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TileKey {
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

/// Web Mercator：经纬度（GCJ-02）→ 世界像素（zoom z，256 瓦片）。
pub fn lonlat_to_world(lon: f64, lat: f64, z: f64) -> (f64, f64) {
    let n = TILE_PX * 2f64.powf(z);
    let x = (lon + 180.0) / 360.0 * n;
    let lat_rad = lat.to_radians();
    let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// Web Mercator 逆变换：世界像素 → (lat, lon)。
pub fn world_to_lonlat(x: f64, y: f64, z: f64) -> (f64, f64) {
    let n = TILE_PX * 2f64.powf(z);
    let lon = x / n * 360.0 - 180.0;
    let lat_rad = (std::f64::consts::PI * (1.0 - 2.0 * y / n)).sinh().atan();
    (lat_rad.to_degrees(), lon)
}

fn cache_path(key: TileKey) -> std::path::PathBuf {
    crate::platform::data_dir()
        .join("map_tiles")
        .join(key.z.to_string())
        .join(key.x.to_string())
        .join(format!("{}.png", key.y))
}

/// 瓦片状态：纹理 LRU + 请求队列 + 下载线程句柄（生命周期随 App）。
pub struct TileState {
    textures: HashMap<TileKey, egui::TextureHandle>,
    order: Vec<TileKey>,
    pending: HashSet<TileKey>,
    failed: HashSet<TileKey>,
    req_tx: std::sync::mpsc::Sender<TileKey>,
    done_rx: std::sync::mpsc::Receiver<(TileKey, bool)>,
}

impl TileState {
    /// 创建状态并启动下载线程。
    pub fn new() -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<TileKey>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<(TileKey, bool)>();
        std::thread::Builder::new()
            .name("tile-fetch".into())
            .spawn(move || {
                while let Ok(key) = req_rx.recv() {
                    let ok = fetch_tile(key);
                    if done_tx.send((key, ok)).is_err() {
                        return;
                    }
                }
            })
            .ok();
        Self {
            textures: HashMap::new(),
            order: Vec::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            req_tx,
            done_rx,
        }
    }

    /// 帧内调用：收取下载完成的瓦片并加载为纹理。
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((key, ok)) = self.done_rx.try_recv() {
            self.pending.remove(&key);
            if !ok {
                self.failed.insert(key);
                continue;
            }
            let path = cache_path(key);
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => {
                    self.failed.insert(key);
                    continue;
                }
            };
            let img = match image::load_from_memory(&bytes) {
                Ok(img) => img.to_rgba8(),
                Err(_) => {
                    self.failed.insert(key);
                    continue;
                }
            };
            let (w, h) = (img.width() as usize, img.height() as usize);
            let color = egui::ColorImage::from_rgba_unmultiplied([w, h], img.as_raw());
            let tex = ctx.load_texture(
                format!("tile{}_{}_{}", key.z, key.x, key.y),
                color,
                egui::TextureOptions::LINEAR,
            );
            if let Some(old) = self.textures.insert(key, tex) {
                let _ = old; // 旧纹理由 egui 延迟回收
            }
            self.order.push(key);
            while self.order.len() > MAX_TILES {
                let old_key = self.order.remove(0);
                self.textures.remove(&old_key);
            }
        }
    }

    /// 请求当前视野内的缺失瓦片（防重复/防失败重试）。
    pub fn request_visible(
        &mut self,
        center: (f64, f64),
        zoom: f32,
        offset: egui::Vec2,
        rect: egui::Rect,
    ) {
        let zt = zoom.round().clamp(ZOOM_MIN, ZOOM_MAX) as u32;
        let (wx, wy) = lonlat_to_world(center.1, center.0, zt as f64);
        let c = rect.center();
        // 屏幕左上角对应的世界像素
        let left = wx - (c.x - rect.left()) as f64 + offset.x as f64;
        let top = wy - (c.y - rect.top()) as f64 + offset.y as f64;
        let x0 = (left / TILE_PX).floor() as i64;
        let y0 = (top / TILE_PX).floor() as i64;
        let n = (rect.width() as f64 / TILE_PX).ceil() as i64 + 2;
        let m = (rect.height() as f64 / TILE_PX).ceil() as i64 + 2;
        let max_tile = (1i64 << zt) - 1;
        for dy in 0..m {
            for dx in 0..n {
                let x = x0 + dx;
                let y = y0 + dy;
                if x < 0 || y < 0 || x > max_tile || y > max_tile {
                    continue;
                }
                let key = TileKey { z: zt, x: x as u32, y: y as u32 };
                if self.textures.contains_key(&key)
                    || self.pending.contains(&key)
                    || self.failed.contains(&key)
                {
                    continue;
                }
                self.pending.insert(key);
                if self.req_tx.send(key).is_err() {
                    self.pending.remove(&key);
                    return;
                }
            }
        }
    }

    /// 绘制可见瓦片（世界像素 → 屏幕矩形）。
    pub fn draw(
        &self,
        painter: &egui::Painter,
        center: (f64, f64),
        zoom: f32,
        offset: egui::Vec2,
        rect: egui::Rect,
    ) {
        let zt = zoom.round().clamp(ZOOM_MIN, ZOOM_MAX) as u32;
        let scale = 2f64.powf((zoom - zt as f32) as f64);
        let (wx, wy) = lonlat_to_world(center.1, center.0, zt as f64);
        let c = rect.center();
        let left = wx - (c.x - rect.left()) as f64 + offset.x as f64;
        let top = wy - (c.y - rect.top()) as f64 + offset.y as f64;
        let x0 = (left / TILE_PX).floor() as i64;
        let y0 = (top / TILE_PX).floor() as i64;
        let n = (rect.width() as f64 / TILE_PX).ceil() as i64 + 2;
        let m = (rect.height() as f64 / TILE_PX).ceil() as i64 + 2;
        let max_tile = (1i64 << zt) - 1;
        for dy in 0..m {
            for dx in 0..n {
                let x = x0 + dx;
                let y = y0 + dy;
                if x < 0 || y < 0 || x > max_tile || y > max_tile {
                    continue;
                }
                let key = TileKey { z: zt, x: x as u32, y: y as u32 };
                if let Some(tex) = self.textures.get(&key) {
                    let sx = (x as f64 * TILE_PX - left) * scale + rect.left() as f64;
                    let sy = (y as f64 * TILE_PX - top) * scale + rect.top() as f64;
                    let size = (TILE_PX * scale) as f32;
                    painter.image(
                        tex.id(),
                        egui::Rect::from_min_size(
                            egui::Pos2::new(sx as f32, sy as f32),
                            egui::Vec2::splat(size),
                        ),
                        egui::Rect::from_min_max(
                            egui::Pos2::ZERO,
                            egui::Pos2::new(1.0, 1.0),
                        ),
                        egui::Color32::WHITE,
                    );
                }
            }
        }
    }
}

/// 下载瓦片并写磁盘缓存；已缓存直接命中。
fn fetch_tile(key: TileKey) -> bool {
    let path = cache_path(key);
    if path.is_file() {
        return true;
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(12))
        .build();
    let url = amap_tile_url(key.x, key.y, key.z);
    let resp = match agent
        .get(&url)
        .set("User-Agent", "Mozilla/5.0 NekoSportsWorldTool/0.3")
        .call()
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    let mut r = resp.into_reader();
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(&mut r, &mut bytes).is_err() || bytes.len() < 200 {
        return false;
    }
    // 校验是合法 PNG（高德越界瓦片返回小图或非图数据）
    if bytes.len() < 4 || &bytes[0..4] != b"\x89PNG" {
        return false;
    }
    let dir = path.parent().unwrap().to_path_buf();
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    std::fs::write(&path, &bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mercator_roundtrip() {
        let (x, y) = lonlat_to_world(121.54, 38.90, 15.0);
        let (lat, lon) = world_to_lonlat(x, y, 15.0);
        assert!((lat - 38.90).abs() < 1e-9, "lat={lat}");
        assert!((lon - 121.54).abs() < 1e-9, "lon={lon}");
    }

    #[test]
    fn tile_url_has_valid_domain() {
        let url = amap_tile_url(10, 20, 12);
        assert!(url.contains("webrd0"));
        assert!(url.contains("x=10"));
        assert!(url.contains("y=20"));
        assert!(url.contains("z=12"));
    }
}
