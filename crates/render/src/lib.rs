//! Peephole renderer: turns a [`Page`] plus the [`Store`] into a frame.
//! Implementation pending; the API below is the contract the hub builds against.

use chrono::{DateTime, Utc};
use peephole_core::{Page, Store};

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
        for px in self.rgba.chunks_exact(4) {
            let v: u16 = ((px[0] as u16 & 0xF8) << 8) | ((px[1] as u16 & 0xFC) << 3) | (px[2] as u16 >> 3);
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }
}

/// Render `page` at `width` × `height`. Pages are designed at `page.width` × `page.height`
/// and scaled to the requested size.
pub fn render_page(page: &Page, store: &Store, ctx: &RenderCtx, width: u32, height: u32) -> Frame {
    let _ = (page, store, ctx);
    Frame::blank(width, height)
}

/// The "hub is up, no page configured" and "device unknown" screens use this.
pub fn render_notice(title: &str, body: &str, width: u32, height: u32) -> Frame {
    let _ = (title, body);
    Frame::blank(width, height)
}
