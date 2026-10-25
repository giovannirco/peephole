//! Peephole renderer: turns a [`Page`] plus the [`Store`] into a frame.
//!
//! Pages are drawn with tiny-skia at their design size in the design system's colours and
//! Roboto (fontdue, anti-aliased), then resampled to the requested size.

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

/// Render `page` at `width` × `height`. Pages are designed at `page.width` × `page.height`
/// and scaled to the requested size.
pub fn render_page(page: &Page, store: &Store, ctx: &RenderCtx, width: u32, height: u32) -> Frame {
    let mut cv = Canvas::new(page.width, page.height);
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
}
