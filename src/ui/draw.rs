//! 手绘路线页：在高德瓦片地图上点选路径（FakeLocation 式）。
//!
//! - 单击地图添加途经点；拖拽平移；滚轮/双指缩放（指针为中心）。
//! - 支持撤销 / 清空 / 自动闭合（首尾相连成环）。
//! - 「保存为跑步路线」写入 drawn_route.json（GCJ-02），跑步页选「手绘路线」模式即可沿此路径跑。

use super::map::MapState;
use super::{mobile, theme, App};
use eframe::egui;
use egui::{Pos2, Sense};

#[derive(Default)]
pub struct DrawPage {
    /// 途经点（GCJ-02，lat, lng），与高德瓦片同基准。
    pub points: Vec<(f64, f64)>,
    pub map: MapState,
    pub fitted: bool,
    pub auto_close: bool,
    pub status: String,
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

impl App {
    pub fn draw_draw_page(&mut self, ui: &mut egui::Ui) {
        let mut want_run_tab = false;
        let mut want_save = false;
        mobile::row(ui, |ui| {
            if ui.button("撤销").clicked() {
                self.draw_page.points.pop();
            }
            if ui.button("清空").clicked() {
                self.draw_page.points.clear();
                self.draw_page.status = String::new();
            }
            ui.checkbox(&mut self.draw_page.auto_close, "自动闭合（绕圈）");
            if ui.button("保存为跑步路线").clicked() {
                want_save = true;
            }
            if ui.button("去跑步").clicked() {
                want_run_tab = true;
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label(&self.draw_page.status);
            let n = self.draw_page.points.len();
            let len = polyline_length(&self.draw_page.points, self.draw_page.auto_close);
            if n > 0 {
                ui.label(format!("{} 点 · 全长 {:.0} m", n, len));
            } else {
                ui.label("点击地图添加途经点");
            }
        });
        ui.add_space(4.0);

        let rect = ui.available_rect_before_wrap();
        let height = (rect.height().max(320.0)).min(ui.available_height().max(320.0));
        let (response, painter) = ui.allocate_painter(
            egui::Vec2::new(rect.width().max(200.0), height),
            Sense::click_and_drag(),
        );
        let canvas = response.rect;
        painter.rect_filled(
            canvas,
            egui::Rounding::ZERO,
            egui::Color32::from_rgb(234, 238, 236),
        );

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

        // 单击（非拖拽）添加点
        if response.clicked() && !self.draw_page.map.dragged {
            if let Some(pos) = response.interact_pointer_pos() {
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

        // 绘制折线与点
        let route_c = egui::Color32::from_rgb(30, 111, 216);
        let n = self.draw_page.points.len();
        if n >= 2 {
            self.draw_page
                .map
                .draw_polyline(&painter, canvas, &self.draw_page.points, route_c, 3.0);
        }
        if self.draw_page.auto_close && n > 2 {
            // 闭合段虚线
            let close = vec![self.draw_page.points[n - 1], self.draw_page.points[0]];
            let p0 = self.draw_page.map.to_screen(canvas, close[0].0, close[0].1);
            let p1 = self.draw_page.map.to_screen(canvas, close[1].0, close[1].1);
            painter.add(egui::Shape::dashed_line(
                &[p0, p1],
                egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(120, 140, 134)),
                6.0,
                4.0,
            ));
        }
        for (i, &(la, lo)) in self.draw_page.points.iter().enumerate() {
            let color = if i == 0 {
                egui::Color32::from_rgb(22, 160, 90)
            } else if i == n - 1 {
                egui::Color32::from_rgb(220, 38, 38)
            } else {
                route_c
            };
            self.draw_page
                .map
                .draw_point(&painter, canvas, la, lo, color, 5.0);
            if i == 0 || i == n - 1 {
                let p = self.draw_page.map.to_screen(canvas, la, lo);
                painter.text(
                    Pos2::new(p.x + 8.0, p.y - 8.0),
                    egui::Align2::LEFT_BOTTOM,
                    if i == 0 { "起点" } else { "终点" },
                    egui::FontId::proportional(11.0),
                    color,
                );
            }
        }

        if want_save {
            self.save_drawn_route();
        }
        if want_run_tab {
            self.tab = 0;
        }
    }

    fn save_drawn_route(&mut self) {
        match crate::track::custom::save_drawn_route(&self.draw_page.points, self.draw_page.auto_close) {
            Ok(()) => {
                let len = polyline_length(&self.draw_page.points, self.draw_page.auto_close);
                self.draw_page.status = format!("√ 已保存为跑步路线（{:.0} m）", len);
            }
            Err(e) => {
                self.draw_page.status = format!("保存失败：{e}");
            }
        }
    }
}
