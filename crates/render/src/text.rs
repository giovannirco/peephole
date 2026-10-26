use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use fontdue::{FontSettings, Metrics};
use tiny_skia::{ColorU8, IntRect, Pixmap};

static REGULAR: LazyLock<fontdue::Font> = LazyLock::new(|| load(include_bytes!("../fonts/Roboto-Regular.ttf")));
static MEDIUM: LazyLock<fontdue::Font> = LazyLock::new(|| load(include_bytes!("../fonts/Roboto-Medium.ttf")));
static BOLD: LazyLock<fontdue::Font> = LazyLock::new(|| load(include_bytes!("../fonts/Roboto-Bold.ttf")));

type Glyph = Arc<(Metrics, Vec<u8>)>;
type GlyphCache = Mutex<HashMap<(Font, u32, char), Glyph>>;
static GLYPHS: LazyLock<GlyphCache> = LazyLock::new(Default::default);

fn load(bytes: &'static [u8]) -> fontdue::Font {
    fontdue::Font::from_bytes(bytes, FontSettings::default()).expect("bundled Roboto font parses")
}

/// The four sizes of the design system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Font {
    /// Bold 44 px.
    Clock,
    /// Bold 26 px.
    Num,
    /// Medium 15 px.
    Mid,
    /// Regular 11 px.
    Small,
}

impl Font {
    /// `clock`, `num` (or `big`), `mid`, `small`.
    pub fn from_name(name: &str) -> Option<Font> {
        match name {
            "clock" => Some(Font::Clock),
            "num" | "big" => Some(Font::Num),
            "mid" => Some(Font::Mid),
            "small" => Some(Font::Small),
            _ => None,
        }
    }

    fn px(self) -> f32 {
        match self {
            Font::Clock => 44.0,
            Font::Num => 26.0,
            Font::Mid => 15.0,
            Font::Small => 11.0,
        }
    }

    fn face(self) -> &'static fontdue::Font {
        match self {
            Font::Clock | Font::Num => &BOLD,
            Font::Mid => &MEDIUM,
            Font::Small => &REGULAR,
        }
    }

    /// Distance from the line box top to the baseline, in px.
    pub fn ascent(self) -> f32 {
        self.face().horizontal_line_metrics(self.px()).expect("Roboto has horizontal metrics").ascent
    }

    /// Line box height (ascent + descent), in px.
    pub fn height(self) -> i32 {
        let m = self.face().horizontal_line_metrics(self.px()).expect("Roboto has horizontal metrics");
        (m.ascent - m.descent).ceil() as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

impl Align {
    /// `left`, `center`, `right`.
    pub fn from_name(name: &str) -> Option<Align> {
        match name {
            "left" => Some(Align::Left),
            "center" => Some(Align::Center),
            "right" => Some(Align::Right),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub font: Font,
    pub color: ColorU8,
    pub align: Align,
}

fn glyph(font: Font, scale: f32, ch: char) -> Glyph {
    let mut cache = GLYPHS.lock().expect("glyph cache poisoned");
    cache
        .entry((font, scale.to_bits(), ch))
        .or_insert_with(|| Arc::new(font.face().rasterize(ch, font.px() * scale)))
        .clone()
}

fn advance(font: Font, s: &str) -> f32 {
    let face = font.face();
    let mut pen = 0.0;
    let mut prev = None;
    for ch in s.chars() {
        if let Some(p) = prev {
            pen += face.horizontal_kern(p, ch, font.px()).unwrap_or(0.0);
        }
        pen += glyph(font, 1.0, ch).0.advance_width;
        prev = Some(ch);
    }
    pen
}

/// Width of `s` in design-space px at `font`, whatever scale it is drawn at.
pub fn text_width(font: Font, s: &str) -> i32 {
    advance(font, s).ceil() as i32
}

/// Break `s` into lines no wider than `max_w` at word boundaries (and at `\n`);
/// a single word wider than `max_w` stays on its own line.
pub fn wrap(font: Font, s: &str, max_w: i32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in s.lines() {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() { word.to_owned() } else { format!("{line} {word}") };
            if !line.is_empty() && text_width(font, &candidate) > max_w {
                lines.push(std::mem::replace(&mut line, word.to_owned()));
            } else {
                line = candidate;
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
}

/// Draw `s` with its line box top at `y`; `x` is the left, centre or right edge per `style.align`.
/// `x`, `y` and `clip` are in design space; glyphs are rasterized at `scale` times the font size
/// and placed at `scale` times their design position. Pixels outside `clip` (when given) are left
/// untouched. The pixmap must be opaque.
pub fn draw_text(pm: &mut Pixmap, x: i32, y: i32, style: &Style, s: &str, clip: Option<IntRect>, scale: f32) {
    let font = style.font;
    let width = advance(font, s);
    let left = match style.align {
        Align::Left => x as f32,
        Align::Center => x as f32 - width / 2.0,
        Align::Right => x as f32 - width,
    };
    let px_size = font.px() * scale;
    let face = font.face();
    let ascent = face.horizontal_line_metrics(px_size).expect("Roboto has horizontal metrics").ascent;
    let baseline = y as f32 * scale + ascent;
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let at = |v: i32| (v as f32 * scale).round() as i32;
    let (cx0, cy0, cx1, cy1) = match clip {
        Some(c) => (at(c.left()).max(0), at(c.top()).max(0), at(c.right()).min(pw), at(c.bottom()).min(ph)),
        None => (0, 0, pw, ph),
    };
    let data = pm.data_mut();
    let mut pen = left * scale;
    let mut prev = None;
    for ch in s.chars() {
        if let Some(p) = prev {
            pen += face.horizontal_kern(p, ch, px_size).unwrap_or(0.0);
        }
        let g = glyph(font, scale, ch);
        let (m, bitmap) = (&g.0, &g.1);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = (baseline - (m.height as i32 + m.ymin) as f32).round() as i32;
        for (row, line) in bitmap.chunks_exact(m.width.max(1)).enumerate().take(m.height) {
            let py = gy + row as i32;
            if py < cy0 || py >= cy1 {
                continue;
            }
            for (col, &cov) in line.iter().enumerate() {
                let px = gx + col as i32;
                if cov == 0 || px < cx0 || px >= cx1 {
                    continue;
                }
                let i = ((py * pw + px) * 4) as usize;
                blend(&mut data[i..i + 3], style.color, cov);
            }
        }
        pen += m.advance_width;
        prev = Some(ch);
    }
}

/// Blend an opaque colour at coverage `a` (0–255) over an opaque RGB pixel.
pub fn blend(px: &mut [u8], c: ColorU8, a: u8) {
    let a = a as u32;
    let mix = |d: u8, s: u8| ((s as u32 * a + d as u32 * (255 - a) + 127) / 255) as u8;
    px[0] = mix(px[0], c.red());
    px[1] = mix(px[1], c.green());
    px[2] = mix(px[2], c.blue());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{BG, TEXT};

    fn canvas(w: u32, h: u32) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(BG.red(), BG.green(), BG.blue(), 255));
        pm
    }

    fn lit(pm: &Pixmap, x0: u32, x1: u32) -> usize {
        let w = pm.width();
        pm.pixels()
            .iter()
            .enumerate()
            .filter(|(i, p)| {
                let x = *i as u32 % w;
                x >= x0 && x < x1 && p.red() > BG.red() + 40
            })
            .count()
    }

    #[test]
    fn width_grows_with_text_and_size() {
        assert_eq!(text_width(Font::Small, ""), 0);
        let small = text_width(Font::Small, "80,838");
        let num = text_width(Font::Num, "80,838");
        assert!(small > 20 && small < 40, "{small}");
        assert!(num > small * 2, "{num} vs {small}");
        assert!(text_width(Font::Mid, "abcd") > text_width(Font::Mid, "ab"));
    }

    #[test]
    fn font_names_map_to_fonts() {
        assert_eq!(Font::from_name("clock"), Some(Font::Clock));
        assert_eq!(Font::from_name("num"), Some(Font::Num));
        assert_eq!(Font::from_name("big"), Some(Font::Num));
        assert_eq!(Font::from_name("mid"), Some(Font::Mid));
        assert_eq!(Font::from_name("small"), Some(Font::Small));
        assert_eq!(Font::from_name("huge"), None);
    }

    #[test]
    fn draws_anti_aliased_text_inside_its_box() {
        let mut pm = canvas(120, 40);
        let style = Style { font: Font::Mid, color: TEXT, align: Align::Left };
        draw_text(&mut pm, 10, 5, &style, "Hello", None, 1.0);
        assert!(lit(&pm, 10, 120) > 30);
        assert_eq!(lit(&pm, 0, 10), 0);
        let partial = pm.pixels().iter().filter(|p| p.red() > BG.red() + 10 && p.red() < TEXT.red() - 30).count();
        assert!(partial > 5, "expected anti-aliased edge pixels, got {partial}");
        assert!(pm.pixels().iter().all(|p| p.alpha() == 255));
    }

    #[test]
    fn right_and_centre_alignment_anchor_at_x() {
        let mut pm = canvas(120, 30);
        draw_text(&mut pm, 110, 5, &Style { font: Font::Mid, color: TEXT, align: Align::Right }, "88", None, 1.0);
        assert_eq!(lit(&pm, 111, 120), 0);
        assert!(lit(&pm, 90, 111) > 10);
        let mut pm = canvas(120, 30);
        draw_text(&mut pm, 60, 5, &Style { font: Font::Mid, color: TEXT, align: Align::Center }, "8888", None, 1.0);
        assert!(lit(&pm, 40, 60) > 5 && lit(&pm, 60, 80) > 5);
    }

    #[test]
    fn clip_rect_limits_the_drawn_pixels() {
        let mut pm = canvas(120, 30);
        let clip = IntRect::from_xywh(20, 0, 30, 30);
        draw_text(&mut pm, 0, 5, &Style { font: Font::Mid, color: TEXT, align: Align::Left }, "WWWWWWWWWW", clip, 1.0);
        assert_eq!(lit(&pm, 0, 20), 0);
        assert_eq!(lit(&pm, 50, 120), 0);
        assert!(lit(&pm, 20, 50) > 10);
    }

    #[test]
    fn text_top_is_at_y() {
        let mut pm = canvas(60, 60);
        draw_text(&mut pm, 2, 20, &Style { font: Font::Small, color: TEXT, align: Align::Left }, "Hg", None, 1.0);
        let rows: Vec<u32> = pm
            .pixels()
            .iter()
            .enumerate()
            .filter(|(_, p)| p.red() > BG.red() + 40)
            .map(|(i, _)| i as u32 / 60)
            .collect();
        let (top, bottom) = (*rows.iter().min().unwrap(), *rows.iter().max().unwrap());
        assert!((20..=24).contains(&top), "top {top}");
        assert!(bottom <= 20 + Font::Small.height() as u32, "bottom {bottom}");
    }

    #[test]
    fn wraps_words_to_the_width() {
        let lines = wrap(Font::Small, "the quick brown fox jumps over the lazy dog", 60);
        assert!(lines.len() > 2, "{lines:?}");
        assert!(lines.iter().all(|l| text_width(Font::Small, l) <= 60), "{lines:?}");
        assert_eq!(lines.join(" "), "the quick brown fox jumps over the lazy dog");
        assert_eq!(wrap(Font::Small, "", 60), Vec::<String>::new());
        assert_eq!(wrap(Font::Small, "supercalifragilistic", 10), vec!["supercalifragilistic"]);
        assert_eq!(wrap(Font::Small, "a\nb", 200), vec!["a", "b"]);
    }

    #[test]
    fn colour_is_kept() {
        let mut pm = canvas(60, 30);
        let c = ColorU8::from_rgba(255, 0, 0, 255);
        draw_text(&mut pm, 2, 2, &Style { font: Font::Num, color: c, align: Align::Left }, "8", None, 1.0);
        assert!(pm.pixels().iter().any(|p| p.red() == 255 && p.green() == 0));
    }

    #[test]
    fn glyph_cache_keeps_sizes_apart() {
        let one = glyph(Font::Mid, 1.0, 'M');
        let two = glyph(Font::Mid, 2.0, 'M');
        assert!(two.0.height > one.0.height, "{} vs {}", two.0.height, one.0.height);
        assert!(two.0.advance_width > one.0.advance_width * 1.9);
    }

    fn lit_span(pm: &Pixmap) -> (u32, u32, u32) {
        let w = pm.width();
        let lit: Vec<(u32, u32)> = pm
            .pixels()
            .iter()
            .enumerate()
            .filter(|(_, p)| p.red() > BG.red() + 40)
            .map(|(i, _)| (i as u32 % w, i as u32 / w))
            .collect();
        let left = lit.iter().map(|p| p.0).min().unwrap();
        let right = lit.iter().map(|p| p.0).max().unwrap();
        let top = lit.iter().map(|p| p.1).min().unwrap();
        (left, right, top)
    }

    #[test]
    fn scaled_text_keeps_the_design_layout() {
        let before = text_width(Font::Mid, "Hello");
        let mut one = canvas(120, 40);
        let style = Style { font: Font::Mid, color: TEXT, align: Align::Left };
        draw_text(&mut one, 10, 5, &style, "Hello", None, 1.0);
        let mut two = canvas(240, 80);
        draw_text(&mut two, 10, 5, &style, "Hello", None, 2.0);
        assert_eq!(text_width(Font::Mid, "Hello"), before, "measurement stays in design space");
        let (l1, r1, t1) = lit_span(&one);
        let (l2, r2, t2) = lit_span(&two);
        assert!((l2 as i32 - 2 * l1 as i32).abs() <= 2, "left {l2} vs 2 x {l1}");
        assert!((r2 as i32 - 2 * r1 as i32).abs() <= 3, "right {r2} vs 2 x {r1}");
        assert!((t2 as i32 - 2 * t1 as i32).abs() <= 2, "top {t2} vs 2 x {t1}");
        let mut right = canvas(240, 80);
        draw_text(&mut right, 110, 5, &Style { align: Align::Right, ..style }, "Hello", None, 2.0);
        assert!((lit_span(&right).1 as i32 - 220).abs() <= 3, "right-aligned at 2 x 110");
    }

    #[test]
    fn clip_rect_scales_with_the_text() {
        let mut pm = canvas(240, 60);
        let clip = IntRect::from_xywh(20, 0, 30, 30);
        draw_text(&mut pm, 0, 5, &Style { font: Font::Mid, color: TEXT, align: Align::Left }, "WWWWWWWWWW", clip, 2.0);
        assert_eq!(lit(&pm, 0, 40), 0);
        assert_eq!(lit(&pm, 100, 240), 0);
        assert!(lit(&pm, 40, 100) > 40);
    }
}
