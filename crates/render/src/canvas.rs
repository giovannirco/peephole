use std::f32::consts::PI;

use tiny_skia::{ColorU8, FillRule, IntRect, LineCap, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::color::{BG, CARD, EDGE};
use crate::text::{self, Align, Font, Style};

/// A page-sized opaque pixmap with the design system's primitives. Every primitive takes
/// design-space coordinates and sizes; `scale` maps them to pixmap pixels.
pub struct Canvas {
    pub pixmap: Pixmap,
    pub scale: f32,
}

fn paint(c: ColorU8, alpha: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(tiny_skia::Color::from_rgba8(c.red(), c.green(), c.blue(), (alpha * 255.0).round() as u8));
    p.anti_alias = true;
    p
}

impl Canvas {
    pub fn new(w: u32, h: u32) -> Self {
        Self::new_scaled(w, h, 1.0)
    }

    /// A `w` × `h` pixel canvas that draws design space at `scale` pixels per unit.
    pub fn new_scaled(w: u32, h: u32, scale: f32) -> Self {
        let mut pixmap = Pixmap::new(w.max(1), h.max(1)).expect("canvas size is non-zero");
        pixmap.fill(tiny_skia::Color::from_rgba8(BG.red(), BG.green(), BG.blue(), 255));
        Self { pixmap, scale }
    }

    /// Width in design space.
    pub fn width(&self) -> i32 {
        (self.pixmap.width() as f32 / self.scale).round() as i32
    }

    /// Height in design space.
    pub fn height(&self) -> i32 {
        (self.pixmap.height() as f32 / self.scale).round() as i32
    }

    fn transform(&self) -> Transform {
        Transform::from_scale(self.scale, self.scale)
    }

    fn fill_path(&mut self, path: &Path, c: ColorU8, alpha: f32) {
        let t = self.transform();
        self.pixmap.fill_path(path, &paint(c, alpha), FillRule::Winding, t, None);
    }

    fn stroke_path(&mut self, path: &Path, c: ColorU8, width: f32, cap: LineCap) {
        let stroke = Stroke { width, line_cap: cap, ..Stroke::default() };
        let t = self.transform();
        self.pixmap.stroke_path(path, &paint(c, 1.0), &stroke, t, None);
    }

    /// Pixel-exact filled rectangle (no anti-aliasing); its edges round to whole scaled pixels.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: ColorU8) {
        let at = |v: i32| (v as f32 * self.scale).round();
        let (x0, y0) = (at(x), at(y));
        let Some(r) = Rect::from_xywh(x0, y0, at(x + w) - x0, at(y + h) - y0) else { return };
        let mut p = paint(c, 1.0);
        p.anti_alias = false;
        self.pixmap.fill_rect(r, &p, Transform::identity(), None);
    }

    pub fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: ColorU8) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let r = r.min(w / 2.0).min(h / 2.0);
        let (x1, y1) = (x + w, y + h);
        let mut pb = PathBuilder::new();
        pb.move_to(x + r, y);
        pb.line_to(x1 - r, y);
        pb.quad_to(x1, y, x1, y + r);
        pb.line_to(x1, y1 - r);
        pb.quad_to(x1, y1, x1 - r, y1);
        pb.line_to(x + r, y1);
        pb.quad_to(x, y1, x, y1 - r);
        pb.line_to(x, y + r);
        pb.quad_to(x, y, x + r, y);
        pb.close();
        if let Some(path) = pb.finish() {
            self.fill_path(&path, c, 1.0);
        }
    }

    /// Card fill with a 1 px `edge` border (radius 6) and a 3 px accent bar inset on the left edge.
    pub fn card(&mut self, x: i32, y: i32, w: i32, h: i32, accent: Option<ColorU8>) {
        let (fx, fy, fw, fh) = (x as f32, y as f32, w as f32, h as f32);
        self.rrect(fx, fy, fw, fh, 6.0, EDGE);
        self.rrect(fx + 1.0, fy + 1.0, fw - 2.0, fh - 2.0, 5.0, CARD);
        if let Some(a) = accent {
            let inset = 8.min((h - 2) / 4);
            self.fill_rect(x + 1, y + inset, 3, h - 2 * inset, a);
        }
    }

    pub fn dot(&mut self, cx: f32, cy: f32, r: f32, c: ColorU8) {
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            self.fill_path(&path, c, 1.0);
        }
    }

    /// Part of the upper semicircle centred on `(cx, cy)`: `(from, to)` are fractions of the
    /// half turn, 0 = left end, 1 = right end, going over the top. The outer edge sits at `r`.
    pub fn arc(&mut self, (cx, cy): (f32, f32), r: f32, width: f32, (from, to): (f32, f32), c: ColorU8) {
        let (from, to) = (from.clamp(0.0, 1.0), to.clamp(0.0, 1.0));
        if to <= from {
            return;
        }
        let radius = r - width / 2.0;
        let steps = ((to - from) * 90.0).ceil().max(1.0) as usize;
        let mut pb = PathBuilder::new();
        for i in 0..=steps {
            let t = from + (to - from) * i as f32 / steps as f32;
            let a = PI + t * PI;
            let (px, py) = (cx + radius * a.cos(), cy + radius * a.sin());
            if i == 0 { pb.move_to(px, py) } else { pb.line_to(px, py) }
        }
        if let Some(path) = pb.finish() {
            self.stroke_path(&path, c, width, LineCap::Butt);
        }
    }

    pub fn polyline(&mut self, pts: &[(f32, f32)], width: f32, c: ColorU8) {
        if pts.len() < 2 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for &(x, y) in &pts[1..] {
            pb.line_to(x, y);
        }
        if let Some(path) = pb.finish() {
            self.stroke_path(&path, c, width, LineCap::Round);
        }
    }

    pub fn polygon(&mut self, pts: &[(f32, f32)], c: ColorU8, alpha: f32) {
        if pts.len() < 3 {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for &(x, y) in &pts[1..] {
            pb.line_to(x, y);
        }
        pb.close();
        if let Some(path) = pb.finish() {
            self.fill_path(&path, c, alpha);
        }
    }

    /// 1 px horizontal line from `x0` to `x1` inclusive, 2 px on / 2 px off.
    pub fn dashed_hline(&mut self, x0: i32, x1: i32, y: i32, c: ColorU8) {
        let mut x = x0;
        while x <= x1 {
            self.fill_rect(x, y, 2.min(x1 - x + 1), 1, c);
            x += 4;
        }
    }

    /// Resample `src` (straight, non-premultiplied RGBA) to the `w` × `h` box at (`x`, `y`) and
    /// alpha-blend it.
    pub fn blit(&mut self, x: i32, y: i32, w: i32, h: i32, src: &image::RgbaImage) {
        let at = |v: i32| (v as f32 * self.scale).round() as i32;
        let (sw, sh) = (at(w).max(1) as u32, at(h).max(1) as u32);
        let img = image::imageops::resize(src, sw, sh, image::imageops::FilterType::Triangle);
        let (x, y) = (at(x), at(y));
        let (pw, ph) = (self.pixmap.width() as i32, self.pixmap.height() as i32);
        let data = self.pixmap.data_mut();
        for (ix, iy, p) in img.enumerate_pixels() {
            let (px, py) = (x + ix as i32, y + iy as i32);
            if px < 0 || py < 0 || px >= pw || py >= ph || p[3] == 0 {
                continue;
            }
            let i = ((py * pw + px) * 4) as usize;
            text::blend(&mut data[i..i + 3], ColorU8::from_rgba(p[0], p[1], p[2], 255), p[3]);
        }
    }

    pub fn text(&mut self, x: i32, y: i32, font: Font, color: ColorU8, align: Align, s: &str) {
        text::draw_text(&mut self.pixmap, x, y, &Style { font, color, align }, s, None, self.scale);
    }

    pub fn text_clipped(&mut self, x: i32, y: i32, style: &Style, s: &str, clip: IntRect) {
        text::draw_text(&mut self.pixmap, x, y, style, s, Some(clip), self.scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{BAD, BG, CARD, EDGE, OK};

    fn at(c: &Canvas, x: u32, y: u32) -> (u8, u8, u8) {
        let p = c.pixmap.pixel(x, y).unwrap();
        (p.red(), p.green(), p.blue())
    }

    fn rgb(c: ColorU8) -> (u8, u8, u8) {
        (c.red(), c.green(), c.blue())
    }

    #[test]
    fn new_canvas_is_opaque_background() {
        let c = Canvas::new(20, 10);
        assert_eq!(at(&c, 0, 0), rgb(BG));
        assert_eq!(at(&c, 19, 9), rgb(BG));
        assert!(c.pixmap.pixels().iter().all(|p| p.alpha() == 255));
    }

    #[test]
    fn rounded_rect_leaves_corners_open() {
        let mut c = Canvas::new(40, 40);
        c.rrect(5.0, 5.0, 30.0, 30.0, 6.0, OK);
        assert_eq!(at(&c, 20, 20), rgb(OK));
        assert_eq!(at(&c, 5, 5), rgb(BG));
        assert_eq!(at(&c, 20, 5), rgb(OK));
    }

    #[test]
    fn card_has_fill_border_and_accent_bar() {
        let mut c = Canvas::new(100, 60);
        c.card(4, 4, 90, 50, Some(BAD));
        assert_eq!(at(&c, 50, 30), rgb(CARD));
        assert_eq!(at(&c, 4, 30), rgb(EDGE));
        assert_eq!(at(&c, 6, 30), rgb(BAD));
        assert_eq!(at(&c, 8, 30), rgb(CARD));
        let mut plain = Canvas::new(100, 60);
        plain.card(4, 4, 90, 50, None);
        assert_eq!(at(&plain, 6, 30), rgb(CARD));
    }

    #[test]
    fn semicircle_arc_is_open_at_the_bottom() {
        let mut c = Canvas::new(60, 60);
        c.arc((30.0, 30.0), 20.0, 4.0, (0.0, 1.0), OK);
        assert_eq!(at(&c, 30, 12), rgb(OK));
        assert_eq!(at(&c, 12, 29), rgb(OK));
        assert_eq!(at(&c, 48, 29), rgb(OK));
        assert_eq!(at(&c, 30, 48), rgb(BG));
        assert_eq!(at(&c, 30, 30), rgb(BG));
    }

    #[test]
    fn partial_arc_starts_on_the_left() {
        let mut c = Canvas::new(60, 60);
        c.arc((30.0, 30.0), 20.0, 4.0, (0.0, 0.25), OK);
        assert_eq!(at(&c, 12, 29), rgb(OK));
        assert_eq!(at(&c, 48, 29), rgb(BG));
        assert_eq!(at(&c, 30, 12), rgb(BG));
    }

    #[test]
    fn dot_and_dashed_line() {
        let mut c = Canvas::new(40, 20);
        c.dot(10.0, 10.0, 3.0, OK);
        assert_eq!(at(&c, 10, 10), rgb(OK));
        c.dashed_hline(0, 39, 18, EDGE);
        let row: Vec<_> = (0..40).map(|x| at(&c, x, 18)).collect();
        assert!(row.contains(&rgb(EDGE)) && row.contains(&rgb(BG)));
    }

    #[test]
    fn fill_rect_is_exact() {
        let mut c = Canvas::new(20, 20);
        c.fill_rect(2, 3, 4, 5, OK);
        assert_eq!(at(&c, 2, 3), rgb(OK));
        assert_eq!(at(&c, 5, 7), rgb(OK));
        assert_eq!(at(&c, 6, 7), rgb(BG));
        assert_eq!(at(&c, 5, 8), rgb(BG));
    }

    #[test]
    fn scaled_canvas_reports_design_size() {
        let c = Canvas::new_scaled(640, 340, 2.0);
        assert_eq!((c.pixmap.width(), c.pixmap.height()), (640, 340));
        assert_eq!((c.width(), c.height()), (320, 170));
        assert_eq!((Canvas::new(320, 170).width(), Canvas::new(320, 170).height()), (320, 170));
    }

    #[test]
    fn fill_rect_lands_on_scaled_pixels() {
        let mut c = Canvas::new_scaled(80, 80, 2.0);
        c.fill_rect(10, 10, 20, 20, OK);
        assert_eq!(at(&c, 20, 20), rgb(OK));
        assert_eq!(at(&c, 19, 19), rgb(BG));
        assert_eq!(at(&c, 59, 59), rgb(OK));
        assert_eq!(at(&c, 60, 60), rgb(BG));
    }

    #[test]
    fn paths_scale_with_the_canvas() {
        let mut c = Canvas::new_scaled(80, 80, 2.0);
        c.rrect(5.0, 5.0, 30.0, 30.0, 6.0, OK);
        assert_eq!(at(&c, 40, 40), rgb(OK));
        assert_eq!(at(&c, 40, 11), rgb(OK));
        assert_eq!(at(&c, 11, 11), rgb(BG));
        assert_eq!(at(&c, 40, 70), rgb(BG));
        let mut d = Canvas::new_scaled(40, 40, 2.0);
        d.dot(10.0, 10.0, 3.0, OK);
        assert_eq!(at(&d, 24, 20), rgb(OK));
        assert_eq!(at(&d, 27, 20), rgb(BG));
    }

    #[test]
    fn stroke_width_scales_once() {
        let mut c = Canvas::new_scaled(120, 120, 2.0);
        c.arc((30.0, 30.0), 20.0, 4.0, (0.0, 1.0), OK);
        assert_eq!(at(&c, 60, 19), rgb(BG));
        assert_eq!(at(&c, 60, 24), rgb(OK));
        assert_eq!(at(&c, 60, 30), rgb(BG));
        assert_eq!(at(&c, 24, 59), rgb(OK));
        let mut l = Canvas::new_scaled(40, 40, 2.0);
        l.polyline(&[(2.0, 10.0), (18.0, 10.0)], 2.0, OK);
        assert_eq!(at(&l, 20, 19), rgb(OK));
        assert_eq!(at(&l, 20, 22), rgb(BG));
    }

    #[test]
    fn blit_fills_the_scaled_box() {
        let red = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
        let mut c = Canvas::new_scaled(40, 40, 2.0);
        c.blit(5, 5, 10, 10, &red);
        assert_eq!(at(&c, 10, 10), (255, 0, 0));
        assert_eq!(at(&c, 29, 29), (255, 0, 0));
        assert_eq!(at(&c, 9, 9), rgb(BG));
        assert_eq!(at(&c, 30, 30), rgb(BG));
    }
}
