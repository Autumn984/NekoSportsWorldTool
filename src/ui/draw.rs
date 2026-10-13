//! 手绘路线页：在高德瓦片地图上点选路径（FakeLocation 式）。
//!
//! - 地图画布占满整页剩余空间；工具栏以半透明面板悬浮在地图顶部。
//! - 单击地图添加途经点；拖拽平移；滚轮/双指缩放（指针为中心）。
//! - 支持撤销 / 清空 / 自动闭合（首尾相连成环）。
//! - 「保存为跑步路线」写入 drawn_route.json（GCJ-02），跑步页选「手绘路线」模式即可沿此路径跑。

use super::map::MapState;
use super::{theme, App};
use eframe::egui;
use egui::{Pos2, Sense};

pub struct DrawPage {
    /// 途经点（GCJ-02，lat, lng），与高德瓦片同基准。
    pub points: Vec<(f64, f64)>,
    pub map: MapState,
    pub fitted: bool,
    pub auto_close: bool,
    pub status: String,
    /// 点击诊断（临时）
    pub debug: String,
    /// 悬浮工具栏占据的屏幕区域（用于排除点击）。
    pub toolbar_rect: egui::Rect,
    /// 路线叠加纹理缓存（脏检查）。
    pub overlay_fp: (u64, u64, u64, u64, u64, u64, u64, u64),
    pub overlay_tex: Option<egui::TextureHandle>,
}

impl Default for DrawPage {
    fn default() -> Self {
        Self {
            points: Vec::new(),
            map: MapState::default(),
            fitted: false,
            auto_close: false,
            status: String::new(),
            debug: String::new(),
            toolbar_rect: egui::Rect::NOTHING,
            overlay_fp: (0, 0, 0, 0, 0, 0, 0, 0),
            overlay_tex: None,
        }
    }
}

/// 折线总长（米）；auto_close 时含闭合段。
pub fn polyline_length(points: &[(f64, f64)], auto_close: bool) -> f64 {
    let mut total = 0.0;
    let n = points.len();
    for i in 0..n.saturating_sub(1) {
        total += dist_m(points[i], points[i + 1]);
    }
    if auto_close && n > 2 {
        total += dist_m(points[n - 1], points[0]);
    }
    total
}

fn dist_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dlat = (a.0 - b.0) * crate::track::geom::MET_PER_DEG_LAT;
    let dlng = (a.1 - b.1) * crate::track::geom::MET_PER_DEG_LNG;
    (dlat * dlat + dlng * dlng).sqrt()
}

/// 在 RGBA 缓冲上做 alpha 混合（src-over）。
fn blend_px(buf: &mut [u8], w: usize, x: usize, y: usize, col: [u8; 3], alpha: u8) {
    let i = (y * w + x) * 4;
    let a = alpha as f32 / 255.0;
    let dst_a = buf[i + 3] as f32 / 255.0;
    let out_a = a + dst_a * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for k in 0..3 {
        let c = (col[k] as f32 * a + buf[i + k] as f32 * dst_a * (1.0 - a)) / out_a;
        buf[i + k] = c as u8;
    }
    buf[i + 3] = (out_a * 255.0) as u8;
}

impl App {
    pub fn draw_draw_page(&mut self, ui: &mut egui::Ui) {
        // 包一层 ScrollArea：本机 Android 上中央面板直接绘制的子控件不渲染/不占位，
        // ScrollArea 子 UI 与跑步页一致，控件正常。
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.draw_draw_page_inner(ui);
        });
    }

    fn draw_draw_page_inner(&mut self, ui: &mut egui::Ui) {
        let mut want_run_tab = false;
        let mut want_save = false;
        let mut want_retry_tiles = false;

        // 工具栏（普通布局，画布上方）
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
            if ui.small_button("撤销").clicked() {
                self.draw_page.points.pop();
            }
            if ui.small_button("清空").clicked() {
                self.draw_page.points.clear();
                self.draw_page.status = String::new();
            }
            ui.checkbox(&mut self.draw_page.auto_close, "闭合");
            if ui.small_button("保存路线").clicked() {
                want_save = true;
            }
            if ui.small_button("去跑步").clicked() {
                want_run_tab = true;
            }
            if ui.small_button("重试瓦片").clicked() {
                want_retry_tiles = true;
            }
        });
        let n = self.draw_page.points.len();
        let smooth = if n >= 2 {
            crate::track::geom::smooth_polyline(&self.draw_page.points, 12, self.draw_page.auto_close)
        } else {
            Vec::new()
        };
        ui.horizontal_wrapped(|ui| {
            let len = polyline_length(&smooth, false);
            let hint = if n > 0 {
                format!("{} 点·全长 {:.0} m", n, len)
            } else {
                "点击地图加点".to_string()
            };
            ui.label(egui::RichText::new(hint).small().color(theme::plain()));
            ui.label(
                egui::RichText::new(&self.draw_page.status)
                    .small()
                    .color(theme::ok()),
            );
            ui.label(
                egui::RichText::new(format!("瓦片 {}·败 {}", self.tiles.loaded, self.tiles.failed_total))
                    .small()
                    .color(theme::text_dim()),
            );
        });
        ui.add_space(2.0);

        // 地图画布：固定高度（ScrollArea 内可滚动页面）
        let full = ui.available_rect_before_wrap();
        let size = egui::Vec2::new(full.width().max(200.0), 430.0);
        let (response, _p) = ui.allocate_painter(size, Sense::click_and_drag());
        let canvas = response.rect;
        let painter = ui.painter();

        // 首次进入：以学校位置（打卡点缓存/锚点）为视野中心
        if !self.draw_page.fitted {
            let base: Vec<(f64, f64)> = self
                .identity
                .anchor_coordinate()
                .ok()
                .and_then(crate::api::model::load_points_cache_for)
                .map(|(_ts, pts)| crate::api::points::points_bd(&pts))
                .unwrap_or_default()
                .iter()
                .map(|&(la, lo)| crate::track::wire::bd09_to_gcj02(la, lo))
                .collect();
            if !base.is_empty() {
                self.draw_page.map.fit(canvas, &base);
            } else {
                self.draw_page.map.center = (38.90, 121.54);
                self.draw_page.map.zoom = 15.0;
            }
            self.draw_page.fitted = true;
        }

        // 瓦片：请求 → 绘制 → 交互
        self.tiles.request_visible(
            self.draw_page.map.center,
            self.draw_page.map.zoom,
            self.draw_page.map.offset,
            canvas,
        );
        self.tiles.draw(
            &painter,
            self.draw_page.map.center,
            self.draw_page.map.zoom,
            self.draw_page.map.offset,
            canvas,
        );
        self.draw_page.map.interact(ui, canvas);

        // 单击添加点：从原始事件里取「主键抬起」的位置（egui 触控抬起后 pointer
        // 位置会被清空，latest_pos/interact_pos 均不可靠）；手势总位移小于阈值才算点击。
        let gesture_drag = response.drag_delta().length();
        let mut release_pos: Option<Pos2> = None;
        ui.input(|i| {
            for e in &i.raw.events {
                if let egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    ..
                } = e
                {
                    release_pos = Some(*pos);
                }
            }
        });
        if let Some(pos) = release_pos {
            if gesture_drag < 6.0 && canvas.contains(pos) {
                let (la, lo) = self.draw_page.map.to_lonlat(canvas, pos);
                let too_close = self
                    .draw_page
                    .points
                    .last()
                    .map(|&last| dist_m(last, (la, lo)) < 3.0)
                    .unwrap_or(false);
                if !too_close {
                    self.draw_page.points.push((la, lo));
                }
            }
        }

        // 软件光栅化路线叠加层（本机 Android 上仅 Image 图元稳定渲染，
        // 几何形状会被丢弃；把路线画进 RGBA 纹理再作为图片贴出）
        self.draw_route_overlay(ui, &painter, canvas, &smooth);

        if want_save {
            self.save_drawn_route();
        }
        if want_run_tab {
            // 去跑步：自动切换到手绘路线模式
            self.run_page.route_mode = crate::track::generate_road::RouteMode::Drawn;
            self.config.route_mode = "drawn".into();
            let _ = crate::api::model::save_config(&self.config);
            self.tab = 0;
        }
        if want_retry_tiles {
            self.tiles.reset_errors();
        }
    }

    /// 软件光栅化路线：把平滑曲线/闭合段/端点标定画进 RGBA 纹理，作为图片贴到画布。
    /// 本机 Android 上几何形状（线/圆/矩形）经 egui painter 均被丢弃，
    /// 只有 Image 图元可靠，因此路线走 CPU 光栅化。
    fn draw_route_overlay(
        &mut self,
        ui: &egui::Ui,
        painter: &egui::Painter,
        canvas: egui::Rect,
        smooth: &[(f64, f64)],
    ) {
        let n = self.draw_page.points.len();
        // 指纹：点数/视野/画布尺寸
        let fp = (
            n as u64,
            self.draw_page.map.center.0.to_bits() as u64,
            self.draw_page.map.center.1.to_bits() as u64,
            self.draw_page.map.zoom.to_bits() as u64,
            self.draw_page.map.offset.x.to_bits() as u64,
            self.draw_page.map.offset.y.to_bits() as u64,
            canvas.width().to_bits() as u64,
            canvas.height().to_bits() as u64,
        );
        if self.draw_page.overlay_fp == fp {
            if let Some(tex) = &self.draw_page.overlay_tex {
                painter.image(
                    tex.id(),
                    canvas,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            return;
        }
        self.draw_page.overlay_fp = fp;
        if n == 0 {
            self.draw_page.overlay_tex = None;
            return;
        }
        // 2× 逻辑分辨率光栅化
        let scale = 2.0f32;
        let w = (canvas.width() * scale).round() as usize;
        let h = (canvas.height() * scale).round() as usize;
        if w < 4 || h < 4 {
            return;
        }
        let to_px = |la: f64, lo: f64| -> (f32, f32) {
            let p = self.draw_page.map.to_screen(canvas, la, lo);
            ((p.x - canvas.min.x) * scale, (p.y - canvas.min.y) * scale)
        };
        // 线段集：(a, b, [r,g,b], 线宽逻辑px)
        let mut segs: Vec<((f32, f32), (f32, f32), [u8; 3], f32)> = Vec::new();
        let blue = [30u8, 111, 216];
        for wd in smooth.windows(2) {
            segs.push((to_px(wd[0].0, wd[0].1), to_px(wd[1].0, wd[1].1), blue, 3.0));
        }
        if self.draw_page.auto_close && n > 2 {
            let a = self.draw_page.points[n - 1];
            let b = self.draw_page.points[0];
            segs.push((to_px(a.0, a.1), to_px(b.0, b.1), [120u8, 140, 134], 1.5));
        }

        let mut buf = vec![0u8; w * h * 4];
        for (a, b, col, width) in &segs {
            let half = (*width as f32 * scale * 0.5).max(1.0);
            let minx = ((a.0.min(b.0) - half - 2.0).floor().max(0.0)) as usize;
            let maxx = (((a.0.max(b.0) + half + 2.0).ceil()) as usize).min(w.saturating_sub(1));
            let miny = ((a.1.min(b.1) - half - 2.0).floor().max(0.0)) as usize;
            let maxy = (((a.1.max(b.1) + half + 2.0).ceil()) as usize).min(h.saturating_sub(1));
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len2 = dx * dx + dy * dy;
            if len2 < 0.01 {
                continue;
            }
            for py in miny..=maxy {
                for px in minx..=maxx {
                    let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
                    let t = (((x - a.0) * dx + (y - a.1) * dy) / len2).clamp(0.0, 1.0);
                    let (qx, qy) = (a.0 + t * dx, a.1 + t * dy);
                    let d = ((x - qx).powi(2) + (y - qy).powi(2)).sqrt();
                    if d > half + 1.0 {
                        continue;
                    }
                    let alpha = if d > half - 1.0 {
                        ((half + 1.0 - d) * 255.0) as u8
                    } else {
                        255
                    };
                    blend_px(&mut buf, w, px, py, *col, alpha);
                }
            }
        }
        // 端点/途经点标定（方块）
        for (i, &(la, lo)) in self.draw_page.points.iter().enumerate() {
            let col = if i == 0 {
                [22u8, 160, 90]
            } else if i == n - 1 {
                [220u8, 38, 38]
            } else {
                blue
            };
            let (cx, cy) = to_px(la, lo);
            let half = (5.0 * scale).max(6.0);
            let (x0, x1) = (
                ((cx - half).floor().max(0.0)) as usize,
                (((cx + half).ceil()) as usize).min(w.saturating_sub(1)),
            );
            let (y0, y1) = (
                ((cy - half).floor().max(0.0)) as usize,
                (((cy + half).ceil()) as usize).min(h.saturating_sub(1)),
            );
            for py in y0..=y1 {
                for px in x0..=x1 {
                    let ddx = px as f32 - cx;
                    let ddy = py as f32 - cy;
                    let d = (ddx * ddx + ddy * ddy).sqrt();
                    let alpha = if d > half - 1.0 {
                        ((half + 1.0 - d) * 255.0) as u8
                    } else {
                        255
                    };
                    blend_px(&mut buf, w, px, py, col, alpha.min(255));
                }
            }
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &buf);
        let tex = ui
            .ctx()
            .load_texture("draw_route_overlay", img, egui::TextureOptions::LINEAR);
        self.draw_page.overlay_tex = Some(tex.clone());
        painter.image(
            tex.id(),
            canvas,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }

    fn save_drawn_route(&mut self) {
        match crate::track::custom::save_drawn_route(&self.draw_page.points, self.draw_page.auto_close) {
            Ok(()) => {
                let smooth = crate::track::geom::smooth_polyline(
                    &self.draw_page.points,
                    12,
                    self.draw_page.auto_close,
                );
                let len = polyline_length(&smooth, false);
                self.draw_page.status = format!("√ 已保存为跑步路线（平滑后 {:.0} m）", len);
            }
            Err(e) => {
                self.draw_page.status = format!("保存失败：{e}");
            }
        }
    }
}
