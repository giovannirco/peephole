//! Peephole renderer: turns a [`Page`] plus the [`Store`] into a frame.
//!
//! Pages are drawn with tiny-skia in the design system's colours and Roboto (fontdue,
//! anti-aliased) at a uniform scale of at least 1 over their design size, then resampled only
//! when that does not land on the requested size.

mod canvas;
mod color;
mod format;
mod text;
mod values;
mod widgets;

use chrono::{DateTime, Utc};
use peephole_core::{Page, Store};

pub use crate::format::{format_age, format_value};

use crate::canvas::Canvas;
use crate::text::{Align, Font};
use crate::values::Lookup;
use crate::widgets::Painter;

/// Context the renderer needs besides the page and the values.
#[derive(Debug, Clone)]
pub struct RenderCtx {
    pub now: DateTime<Utc>,
    /// Values older than this are drawn as stale (grey, with their age).
    pub stale_secs: u64,
    /// `n/total` shown in the header strip.
    pub page_index: usize,
    pub page_count: usize,
    /// The device's message line, for `ticker` widgets without a bind.
    pub message: String,
    /// Per data host: name and whether its last poll succeeded (header dots).
    pub sources: Vec<(String, bool)>,
}

/// RGBA8 pixels, row-major, `width * height * 4` bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn blank(width: u32, height: u32) -> Self {
        Self { width, height, rgba: vec![0; (width * height * 4) as usize] }
    }

    pub fn to_png(&self) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, self.width, self.height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header()?;
            w.write_image_data(&self.rgba)?;
        }
        Ok(out)
    }

    /// Little-endian RGB565, the ST7789's native format.
    pub fn to_rgb565(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.width * self.height * 2) as usize);
        for px in self.rgba.as_chunks::<4>().0 {
            let v: u16 = ((px[0] as u16 & 0xF8) << 8) | ((px[1] as u16 & 0xFC) << 3) | (px[2] as u16 >> 3);
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// Render `page` at `width` × `height`. Pages are designed at `page.width` × `page.height`;
/// larger targets are drawn at a uniform scale (the smaller of the two ratios) so edges stay
/// sharp, smaller ones are drawn at design size and resampled down. Whatever the drawn size
/// misses of the requested one is resampled.
pub fn render_page(page: &Page, store: &Store, ctx: &RenderCtx, width: u32, height: u32) -> Frame {
    let scale = (width as f32 / page.width as f32).min(height as f32 / page.height as f32).max(1.0);
    let (w, h) = ((page.width as f32 * scale).round() as u32, (page.height as f32 * scale).round() as u32);
    let mut cv = Canvas::new_scaled(w, h, scale);
    let look = Lookup { store, now: ctx.now, stale_secs: ctx.stale_secs };
    Painter { cv: &mut cv, look, ctx }.page(page);
    into_frame(cv, width, height)
}

fn into_frame(cv: Canvas, width: u32, height: u32) -> Frame {
    let (w, h) = (cv.pixmap.width(), cv.pixmap.height());
    let rgba = cv.pixmap.take();
    if (w, h) == (width, height) {
        return Frame { width, height, rgba };
    }
    if width == 0 || height == 0 {
        return Frame::blank(width, height);
    }
    let img = image::RgbaImage::from_raw(w, h, rgba).expect("pixmap buffer is w*h*4");
    let out = image::imageops::resize(&img, width, height, image::imageops::FilterType::Triangle);
    Frame { width, height, rgba: out.into_raw() }
}

/// The "hub is up, no page configured" and "device unknown" screens use this.
pub fn render_notice(title: &str, body: &str, width: u32, height: u32) -> Frame {
    let mut cv = Canvas::new(width, height);
    let (w, h) = (width as i32, height as i32);
    cv.card(8, 8, w - 16, h - 16, None);
    let lines = text::wrap(Font::Small, body, w - 40);
    let line_h = Font::Small.height() + 2;
    let block = Font::Mid.height() + 6 + lines.len() as i32 * line_h;
    let mut y = (h - block) / 2;
    cv.text(w / 2, y, Font::Mid, color::TEXT, Align::Center, title);
    y += Font::Mid.height() + 6;
    for line in &lines {
        cv.text(w / 2, y, Font::Small, color::TEXT2, Align::Center, line);
        y += line_h;
    }
    into_frame(cv, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{BG, CARD};
    use peephole_core::{Card, Widget};

    fn ctx() -> RenderCtx {
        RenderCtx {
            now: Utc::now(),
            stale_secs: 300,
            page_index: 0,
            page_count: 1,
            message: String::new(),
            sources: vec![],
        }
    }

    fn page() -> Page {
        Page { name: "p".into(), title: None, width: 320, height: 170, header: true, cards: vec![] }
    }

    fn px(f: &Frame, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * f.width + x) * 4) as usize;
        (f.rgba[i], f.rgba[i + 1], f.rgba[i + 2], f.rgba[i + 3])
    }

    #[test]
    fn renders_opaque_background_at_page_size() {
        let f = render_page(&page(), &Store::new(), &ctx(), 320, 170);
        assert_eq!((f.width, f.height, f.rgba.len()), (320, 170, 320 * 170 * 4));
        assert_eq!(px(&f, 160, 100), (BG.red(), BG.green(), BG.blue(), 255));
        assert!(f.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn scales_to_the_requested_size() {
        let f = render_page(&page(), &Store::new(), &ctx(), 640, 340);
        assert_eq!((f.width, f.height, f.rgba.len()), (640, 340, 640 * 340 * 4));
        assert_eq!(px(&f, 320, 200), (BG.red(), BG.green(), BG.blue(), 255));
        let small = render_page(&page(), &Store::new(), &ctx(), 160, 85);
        assert_eq!(small.rgba.len(), 160 * 85 * 4);
        assert!(render_page(&page(), &Store::new(), &ctx(), 0, 0).rgba.is_empty());
    }

    #[test]
    fn notice_is_a_centred_card_with_title_and_wrapped_body() {
        let body = "no page is configured for this device yet; add one under pages in the config file";
        let f = render_notice("Peephole", body, 320, 170);
        assert_eq!((f.width, f.height), (320, 170));
        assert_eq!(px(&f, 160, 160), (CARD.red(), CARD.green(), CARD.blue(), 255));
        assert_eq!(px(&f, 2, 2), (BG.red(), BG.green(), BG.blue(), 255));
        let lit_rows: Vec<u32> = (0..170).filter(|&y| (0..320).any(|x| px(&f, x, y).1 > 0x60)).collect();
        let first = *lit_rows.first().unwrap();
        let last = *lit_rows.last().unwrap();
        assert!(last - first > 40, "title plus several body lines: {first}..{last}");
        let lit_cols: Vec<u32> = (0..320).filter(|&x| (0..170).any(|y| px(&f, x, y).1 > 0x60)).collect();
        let (l, r) = (lit_cols[0], *lit_cols.last().unwrap());
        assert!((l as i32 - (319 - r) as i32).abs() < 12, "centred: {l}..{r}");
    }

    fn text_page() -> Page {
        let text = Widget::Text {
            x: 10,
            y: 30,
            text: "Hello MMM 80,838 nodes".into(),
            font: Some("mid".into()),
            color: None,
            align: None,
        };
        let card = Card { x: 10, y: 10, w: 300, h: 100, accent: None, title: None, widgets: vec![text] };
        Page { cards: vec![card], header: false, ..page() }
    }

    fn edges(f: &Frame, y: u32) -> usize {
        let lum = |x: u32| {
            let (r, g, b, _) = px(f, x, y);
            0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
        };
        (0..f.width - 1).filter(|&x| (lum(x) - lum(x + 1)).abs() > 64.0).count()
    }

    #[test]
    fn scaled_render_is_crisper_than_resampling() {
        let p = text_page();
        let one = render_page(&p, &Store::new(), &ctx(), 320, 170);
        let img = image::RgbaImage::from_raw(320, 170, one.rgba).unwrap();
        let resized = image::imageops::resize(&img, 640, 340, image::imageops::FilterType::Triangle);
        let resampled = Frame { width: 640, height: 340, rgba: resized.into_raw() };
        let scaled = render_page(&p, &Store::new(), &ctx(), 640, 340);
        let row = ((40.0 + Font::Mid.ascent()) * 2.0).round() as u32 - 2;
        let (sharp, soft) = (edges(&scaled, row), edges(&resampled, row));
        eprintln!("edge pixels on row {row}: scaled {sharp}, resampled {soft}");
        assert!(sharp > soft, "scaled {sharp} vs resampled {soft}");
    }

    #[test]
    fn other_aspects_still_reach_the_requested_size() {
        let f = render_page(&text_page(), &Store::new(), &ctx(), 700, 340);
        assert_eq!((f.width, f.height, f.rgba.len()), (700, 340, 700 * 340 * 4));
        assert_eq!(px(&f, 350, 330), (BG.red(), BG.green(), BG.blue(), 255));
    }
}
