use peephole_core::page::{RowCol, RowItem};
use peephole_core::{Card, Color, Fmt, Page, Thresholds, Widget};
use tiny_skia::{ColorU8, IntRect};

use crate::RenderCtx;
use crate::canvas::Canvas;
use crate::color::{self, BAD, CARD, EDGE, MUTED, OK, TEXT, TEXT2, mix};
use crate::format::{format_age, format_value};
use crate::text::{Align, Font, Style, text_width};
use crate::values::{Lookup, Reading, bind_item};

const HEADER_H: i32 = 18;
const LABEL_GAP: i32 = 12;
const COL_GAP: i32 = 6;
const LED_W: i32 = 6;

/// Colour from an explicit token, else the threshold state, else `fallback`.
fn pick(color: &Option<Color>, th: &Thresholds, v: Option<f64>, fallback: ColorU8) -> ColorU8 {
    if let Some(c) = color {
        return color::resolve(c);
    }
    v.and_then(|v| th.state(v)).map_or(fallback, color::state)
}

fn finite(s: Vec<f64>) -> Vec<f64> {
    s.into_iter().filter(|v| v.is_finite()).collect()
}

pub struct Painter<'a> {
    pub cv: &'a mut Canvas,
    pub look: Lookup<'a>,
    pub ctx: &'a RenderCtx,
}

impl Painter<'_> {
    pub fn page(&mut self, page: &Page) {
        if page.header {
            self.header(page);
        }
        for card in &page.cards {
            self.card(card);
        }
    }

    fn header(&mut self, page: &Page) {
        let title = page.title.as_deref().unwrap_or(&page.name);
        self.cv.text(6, 1, Font::Mid, TEXT2, Align::Left, title);
        let w = self.cv.width();
        let index = format!("{}/{}", self.ctx.page_index + 1, self.ctx.page_count);
        self.cv.text(w - 4, 3, Font::Small, MUTED, Align::Right, &index);
        let mut x = w - 4 - text_width(Font::Small, &index) - 8;
        for (_, ok) in self.ctx.sources.iter().rev() {
            self.cv.dot(x as f32, 9.0, 2.0, if *ok { OK } else { BAD });
            x -= 8;
        }
        self.cv.fill_rect(0, HEADER_H, w, 1, EDGE);
    }

    fn card(&mut self, card: &Card) {
        let accent = card.accent.as_ref().map(color::resolve);
        self.cv.card(card.x, card.y, card.w, card.h, accent);
        if let Some(t) = &card.title {
            self.cv.text(card.x + 8, card.y + 4, Font::Small, TEXT2, Align::Left, t);
        }
        let fallback = accent.unwrap_or(TEXT);
        for w in &card.widgets {
            self.widget(card.x, card.y, w, fallback);
        }
    }

    fn widget(&mut self, ox: i32, oy: i32, w: &Widget, accent: ColorU8) {
        match w {
            Widget::Kpi { x, y, bind, label, unit, fmt, color, thresholds, size } => {
                let font = size.as_deref().and_then(Font::from_name).unwrap_or(Font::Num);
                let kpi = Kpi { label: label.as_deref(), unit: unit.as_deref(), fmt: *fmt, font };
                let c = |v| pick(color, thresholds, Some(v), TEXT);
                self.kpi(ox + x, oy + y, bind, &kpi, c);
            }
            Widget::Gauge { x, y, r, bind, label, color, thresholds } => {
                let reading = self.look.num(bind);
                let c = pick(color, thresholds, reading.clone().value(), accent);
                self.gauge(ox + x, oy + y, *r, reading, c, label.as_deref());
            }
            Widget::Hbar { x, y, w, bind, label, color, thresholds } => {
                let reading = self.look.num(bind);
                let c = pick(color, thresholds, reading.clone().value(), accent);
                self.hbar(ox + x, oy + y, *w, reading, c, label.as_deref());
            }
            Widget::Spark { x, y, w, h, bind, color } => {
                let c = color.as_ref().map_or(accent, color::resolve);
                let reading = self.look.series(bind);
                self.spark(Boxed { x: ox + x, y: oy + y, w: *w, h: *h }, reading, c);
            }
            Widget::Bars { x, y, w, h, bind, color } => {
                let c = color.as_ref().map_or(accent, color::resolve);
                let reading = self.look.series(bind);
                self.bars(Boxed { x: ox + x, y: oy + y, w: *w, h: *h }, reading, c);
            }
            Widget::Text { x, y, text, font, color, align } => {
                let (s, dim) = self.look.expand(text);
                let font = font.as_deref().and_then(Font::from_name).unwrap_or(Font::Small);
                let align = align.as_deref().and_then(Align::from_name).unwrap_or(Align::Left);
                let c = if dim { MUTED } else { color.as_ref().map_or(TEXT, color::resolve) };
                self.cv.text(ox + x, oy + y, font, c, align, &s);
            }
            Widget::Rows { x, y, w, row_h, items, cols } => {
                let pitch = row_h.unwrap_or(20);
                for (i, item) in items.iter().enumerate() {
                    self.row(Boxed { x: ox + x, y: oy + y + i as i32 * pitch, w: *w, h: pitch }, item, cols, accent);
                }
            }
            Widget::Dots { x, y, per_row, items, bind, label } => {
                self.dots(ox + x, oy + y, per_row.unwrap_or(8).max(1), items, bind, label.as_deref());
            }
            Widget::Ticker { x, y, w, bind, color } => {
                let msg = match bind {
                    Some(b) => self.look.text(b).value().unwrap_or_default(),
                    None => self.ctx.message.clone(),
                };
                let c = color.as_ref().map_or(TEXT, color::resolve);
                self.ticker(ox + x, oy + y, *w, &msg, c);
            }
            Widget::Image { x, y, w, h, bind, label } => {
                self.image(Boxed { x: ox + x, y: oy + y, w: *w, h: *h }, bind, label.as_deref());
            }
        }
    }
}

struct Kpi<'a> {
    label: Option<&'a str>,
    unit: Option<&'a str>,
    fmt: Fmt,
    font: Font,
}

#[derive(Clone, Copy)]
struct Boxed {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Painter<'_> {
    fn kpi(&mut self, x: i32, y: i32, bind: &str, k: &Kpi, color: impl Fn(f64) -> ColorU8) {
        let mut vy = y;
        if let Some(l) = k.label {
            self.cv.text(x, y, Font::Small, TEXT2, Align::Left, l);
            vy += LABEL_GAP;
        }
        let (s, c, age) = match self.look.num(bind) {
            Reading::Missing => ("--".to_owned(), MUTED, None),
            Reading::Fresh(v) => (format_value(v, k.fmt), color(v), None),
            Reading::Stale(v, a) => (format_value(v, k.fmt), MUTED, Some(a)),
        };
        self.cv.text(x, vy, k.font, c, Align::Left, &s);
        let mut tx = x + text_width(k.font, &s) + 2;
        let sy = vy + (k.font.ascent() - Font::Small.ascent()).round() as i32;
        if let Some(u) = k.unit {
            self.cv.text(tx, sy, Font::Small, TEXT2, Align::Left, u);
            tx += text_width(Font::Small, u) + 3;
        }
        if let Some(a) = age {
            self.cv.text(tx, sy, Font::Small, MUTED, Align::Left, &format_age(a));
        }
    }

    fn gauge(&mut self, cx: i32, cy: i32, r: i32, reading: Reading<f64>, c: ColorU8, label: Option<&str>) {
        let centre = (cx as f32, cy as f32);
        self.cv.arc(centre, r as f32, 4.0, (0.0, 1.0), EDGE);
        let (text, tc) = match reading {
            Reading::Missing => ("--".to_owned(), MUTED),
            Reading::Fresh(v) | Reading::Stale(v, _) => {
                let fill = if matches!(reading, Reading::Stale(..)) { MUTED } else { c };
                self.cv.arc(centre, r as f32, 4.0, (0.0, v.clamp(0.0, 1.0) as f32), fill);
                (format!("{:.0}", v * 100.0), if fill == MUTED { MUTED } else { TEXT })
            }
        };
        self.cv.text(cx, cy - 2 - Font::Small.height() / 2, Font::Small, tc, Align::Center, &text);
        if let Some(l) = label {
            self.cv.text(cx, cy + 6, Font::Small, TEXT2, Align::Center, l);
        }
    }

    fn meter(&mut self, b: Boxed, reading: &Reading<f64>, c: ColorU8) {
        self.cv.fill_rect(b.x, b.y, b.w, b.h, EDGE);
        let (v, c) = match reading {
            Reading::Missing => return,
            Reading::Fresh(v) => (*v, c),
            Reading::Stale(v, _) => (*v, MUTED),
        };
        let fill = (b.w as f64 * v.clamp(0.0, 1.0)).round() as i32;
        self.cv.fill_rect(b.x, b.y, fill, b.h, c);
    }

    fn hbar(&mut self, x: i32, y: i32, w: i32, reading: Reading<f64>, c: ColorU8, label: Option<&str>) {
        let mut tx = x;
        if let Some(l) = label {
            self.cv.text(x, y, Font::Small, TEXT2, Align::Left, l);
            tx += text_width(Font::Small, l) + COL_GAP;
        }
        let reserve = text_width(Font::Small, "100%") + COL_GAP;
        let track = (x + w - reserve - tx).max(0);
        self.meter(Boxed { x: tx, y: y + 4, w: track, h: 6 }, &reading, c);
        let (s, tc) = match reading {
            Reading::Missing => ("--".to_owned(), MUTED),
            Reading::Fresh(v) => (format_value(v, Fmt::Pct), TEXT),
            Reading::Stale(v, _) => (format_value(v, Fmt::Pct), MUTED),
        };
        self.cv.text(x + w, y, Font::Small, tc, Align::Right, &s);
    }

    fn baseline(&mut self, b: Boxed) {
        self.cv.dashed_hline(b.x, b.x + b.w - 1, b.y + b.h - 1, MUTED);
    }

    fn spark(&mut self, b: Boxed, reading: Reading<Vec<f64>>, c: ColorU8) {
        let (v, c) = match reading {
            Reading::Missing => return self.baseline(b),
            Reading::Fresh(s) => (finite(s), c),
            Reading::Stale(s, _) => (finite(s), MUTED),
        };
        if v.is_empty() {
            return self.baseline(b);
        }
        let lo = v.iter().copied().fold(f64::INFINITY, f64::min);
        let hi = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let (x0, y0, w, h) = (b.x as f32 + 0.5, b.y as f32 + 0.5, (b.w - 1) as f32, (b.h - 1) as f32);
        let y_of = |val: f64| {
            if hi - lo < 1e-12 { y0 + (h / 2.0).floor() } else { y0 + h - ((val - lo) / (hi - lo)) as f32 * h }
        };
        let pts: Vec<(f32, f32)> = if v.len() == 1 {
            vec![(x0, y_of(v[0])), (x0 + w, y_of(v[0]))]
        } else {
            v.iter().enumerate().map(|(i, &val)| (x0 + i as f32 * w / (v.len() - 1) as f32, y_of(val))).collect()
        };
        let bottom = (b.y + b.h) as f32;
        let mut area = pts.clone();
        area.push((pts[pts.len() - 1].0, bottom));
        area.push((pts[0].0, bottom));
        self.cv.polygon(&area, c, 0.25);
        self.cv.polyline(&pts, 1.5, c);
        let (lx, ly) = pts[pts.len() - 1];
        self.cv.dot(lx, ly, 2.0, c);
    }

    fn bars(&mut self, b: Boxed, reading: Reading<Vec<f64>>, c: ColorU8) {
        let (v, c) = match reading {
            Reading::Missing => return self.baseline(b),
            Reading::Fresh(s) => (s, c),
            Reading::Stale(s, _) => (s, MUTED),
        };
        if v.is_empty() {
            return self.baseline(b);
        }
        let n = v.len() as i32;
        let bw = ((b.w - (n - 1)) / n).max(1);
        let hi = v.iter().copied().filter(|x| x.is_finite()).fold(0.0, f64::max);
        let hi = if hi > 0.0 { hi } else { 1.0 };
        let dim = mix(c, CARD, 0.6);
        for (i, &val) in v.iter().enumerate() {
            if !val.is_finite() {
                continue;
            }
            let bh = ((val.max(0.0) / hi * b.h as f64).round() as i32).max(1);
            let col = if i as i32 == n - 1 { c } else { dim };
            self.cv.fill_rect(b.x + i as i32 * (bw + 1), b.y + b.h - bh, bw, bh, col);
        }
    }

    fn row(&mut self, b: Boxed, item: &RowItem, cols: &[RowCol], accent: ColorU8) {
        let mid = b.y + b.h / 2;
        let mut cx = b.x;
        for col in cols {
            match col {
                RowCol::Name { w } => {
                    let name = item.name.as_deref().unwrap_or(&item.key);
                    let top = mid - Font::Mid.height() / 2;
                    if let Some(clip) = IntRect::from_xywh(cx, top, (*w).max(1) as u32, Font::Mid.height() as u32) {
                        let style = Style { font: Font::Mid, color: TEXT, align: Align::Left };
                        self.cv.text_clipped(cx, top, &style, name, clip);
                    }
                    cx += w;
                }
                RowCol::Led { bind, thresholds } => {
                    let c = match self.look.num(&bind_item(bind, &item.key)) {
                        Reading::Fresh(v) => thresholds.state(v).map_or(if v > 0.0 { OK } else { BAD }, color::state),
                        Reading::Missing | Reading::Stale(..) => MUTED,
                    };
                    self.cv.dot((cx + LED_W / 2) as f32, mid as f32, LED_W as f32 / 2.0, c);
                    cx += LED_W;
                }
                RowCol::Num { bind, w, fmt, unit, thresholds } => {
                    let reading = self.look.num(&bind_item(bind, &item.key));
                    let (s, c) = match reading {
                        Reading::Missing => ("--".to_owned(), MUTED),
                        Reading::Fresh(v) => (format_value(v, *fmt), pick(&None, thresholds, Some(v), TEXT)),
                        Reading::Stale(v, _) => (format_value(v, *fmt), MUTED),
                    };
                    let s = match (unit, s.as_str()) {
                        (Some(u), v) if v != "--" => format!("{v} {u}"),
                        _ => s,
                    };
                    self.cv.text(cx + w, mid - Font::Mid.height() / 2, Font::Mid, c, Align::Right, &s);
                    cx += w;
                }
                RowCol::Bar { bind, w, color, thresholds } => {
                    let reading = self.look.num(&bind_item(bind, &item.key));
                    let c = pick(color, thresholds, reading.clone().value(), accent);
                    self.meter(Boxed { x: cx, y: mid - 3, w: *w, h: 6 }, &reading, c);
                    cx += w;
                }
                RowCol::Spark { bind, w, color } => {
                    let c = color.as_ref().map_or(accent, color::resolve);
                    let reading = self.look.series(&bind_item(bind, &item.key));
                    self.spark(Boxed { x: cx, y: b.y + 3, w: *w, h: b.h - 6 }, reading, c);
                    cx += w;
                }
            }
            cx += COL_GAP;
        }
    }

    fn dots(&mut self, x: i32, y: i32, per_row: i32, items: &[RowItem], bind: &str, label: Option<&str>) {
        let mut top = y;
        if let Some(l) = label {
            self.cv.text(x, y, Font::Small, TEXT2, Align::Left, l);
            top += LABEL_GAP + 2;
        }
        for (i, item) in items.iter().enumerate() {
            let (col, row) = (i as i32 % per_row, i as i32 / per_row);
            let c = match self.look.num(&bind_item(bind, &item.key)) {
                Reading::Fresh(v) if v > 0.0 => OK,
                Reading::Fresh(_) => BAD,
                Reading::Missing | Reading::Stale(..) => MUTED,
            };
            self.cv.dot(x as f32 + 3.5 + (col * 10) as f32, top as f32 + 3.5 + (row * 10) as f32, 3.5, c);
        }
    }

    fn ticker(&mut self, x: i32, y: i32, w: i32, msg: &str, c: ColorU8) {
        if msg.is_empty() || w <= 0 {
            return;
        }
        let Some(clip) = IntRect::from_xywh(x, y, w as u32, Font::Mid.height() as u32) else { return };
        let style = Style { font: Font::Mid, color: c, align: Align::Left };
        let tw = text_width(Font::Mid, msg);
        if tw <= w {
            self.cv.text_clipped(x, y, &style, msg, clip);
            return;
        }
        let span = (tw + w) as i64;
        let off = (self.ctx.now.timestamp() * 20).rem_euclid(span) as i32;
        self.cv.text_clipped(x - off, y, &style, msg, clip);
        self.cv.text_clipped(x - off + span as i32, y, &style, msg, clip);
    }

    fn outline(&mut self, b: Boxed) {
        self.cv.fill_rect(b.x, b.y, b.w, 1, EDGE);
        self.cv.fill_rect(b.x, b.y + b.h - 1, b.w, 1, EDGE);
        self.cv.fill_rect(b.x, b.y, 1, b.h, EDGE);
        self.cv.fill_rect(b.x + b.w - 1, b.y, 1, b.h, EDGE);
    }

    fn image(&mut self, b: Boxed, bind: &str, label: Option<&str>) {
        let img = self.look.image(bind).value();
        let fitted = img.filter(|img| img.width > 0 && img.height > 0).and_then(|img| {
            let src = image::RgbaImage::from_raw(img.width, img.height, img.rgba.to_vec())?;
            let scale = (b.w as f64 / img.width as f64).min(b.h as f64 / img.height as f64);
            let (dw, dh) = (
                ((img.width as f64 * scale).round() as u32).max(1),
                ((img.height as f64 * scale).round() as u32).max(1),
            );
            Some(image::imageops::resize(&src, dw, dh, image::imageops::FilterType::Triangle))
        });
        let Some(pic) = fitted else {
            self.outline(b);
            self.cv.text(
                b.x + b.w / 2,
                b.y + (b.h - Font::Small.height()) / 2,
                Font::Small,
                MUTED,
                Align::Center,
                "no image",
            );
            return;
        };
        let (dw, dh) = (pic.width() as i32, pic.height() as i32);
        let (dx, dy) = (b.x + (b.w - dw) / 2, b.y + (b.h - dh) / 2);
        self.cv.blit(dx, dy, &pic);
        self.outline(Boxed { x: dx, y: dy, w: dw, h: dh });
        if let Some(l) = label {
            self.cv.text(dx + 3, dy + 2, Font::Small, TEXT2, Align::Left, l);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use peephole_core::page::{RowCol, RowItem};
    use peephole_core::{Card, Color, Fmt, Page, Store, Thresholds, Value, Widget};

    use crate::color::{BAD, BG, CARD, EDGE, MUTED, OK, TEXT, WARN, mix};
    use crate::{Frame, RenderCtx, render_page};
    use tiny_skia::ColorU8;

    type Rgb = (u8, u8, u8);

    fn rgb(c: ColorU8) -> Rgb {
        (c.red(), c.green(), c.blue())
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap()
    }

    fn ctx() -> RenderCtx {
        RenderCtx {
            now: now(),
            stale_secs: 300,
            page_index: 2,
            page_count: 13,
            message: String::new(),
            sources: vec![],
        }
    }

    fn page(widgets: Vec<Widget>) -> Page {
        Page {
            name: "t".into(),
            title: None,
            width: 320,
            height: 170,
            header: false,
            cards: vec![Card { x: 0, y: 0, w: 320, h: 170, accent: None, title: None, widgets }],
        }
    }

    fn draw(widgets: Vec<Widget>, store: &Store) -> Frame {
        render_page(&page(widgets), store, &ctx(), 320, 170)
    }

    fn px(f: &Frame, x: u32, y: u32) -> Rgb {
        let i = ((y * f.width + x) * 4) as usize;
        (f.rgba[i], f.rgba[i + 1], f.rgba[i + 2])
    }

    fn count(f: &Frame, c: ColorU8, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if px(f, x, y) == rgb(c) {
                    n += 1;
                }
            }
        }
        n
    }

    fn near(f: &Frame, c: ColorU8, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
        let close = |p: Rgb| {
            (p.0 as i32 - c.red() as i32).abs()
                + (p.1 as i32 - c.green() as i32).abs()
                + (p.2 as i32 - c.blue() as i32).abs()
                < 40
        };
        (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).filter(|&(x, y)| close(px(f, x, y))).count()
    }

    fn lit_cols(f: &Frame, y0: u32, y1: u32) -> Option<(u32, u32)> {
        let mut xs = (u32::MAX, 0);
        for y in y0..y1 {
            for x in 0..f.width {
                let p = px(f, x, y);
                if p != rgb(CARD) && p != rgb(EDGE) && p != rgb(BG) {
                    xs = (xs.0.min(x), xs.1.max(x));
                }
            }
        }
        (xs.0 != u32::MAX).then_some(xs)
    }

    fn set(store: &Store, k: &str, v: Value) {
        store.set_at(k, v, now());
    }

    fn kpi(bind: &str, color: Option<&str>, thresholds: Thresholds, size: Option<&str>) -> Widget {
        Widget::Kpi {
            x: 10,
            y: 10,
            bind: bind.into(),
            label: Some("LABEL".into()),
            unit: Some("GB".into()),
            fmt: Fmt::Auto,
            color: color.map(|c| Color(c.into())),
            thresholds,
            size: size.map(Into::into),
        }
    }

    #[test]
    fn kpi_colour_explicit_then_thresholds_then_text() {
        let s = Store::new();
        set(&s, "v", Value::Number(95.0));
        let th = Thresholds { warn: Some(80.0), bad: Some(90.0), invert: false };
        let f = draw(vec![kpi("v", None, th, None)], &s);
        assert!(count(&f, BAD, 0, 20, 160, 60) > 20);
        let f = draw(vec![kpi("v", Some("warn"), th, None)], &s);
        assert!(count(&f, WARN, 0, 20, 160, 60) > 20);
        assert_eq!(count(&f, BAD, 0, 20, 160, 60), 0);
        let f = draw(vec![kpi("v", None, Thresholds::default(), None)], &s);
        assert!(count(&f, TEXT, 0, 20, 160, 60) > 20);
    }

    #[test]
    fn kpi_missing_and_stale_draw_muted_with_age() {
        let s = Store::new();
        let f = draw(vec![kpi("nope", Some("ok"), Thresholds::default(), None)], &s);
        assert!(count(&f, MUTED, 0, 20, 160, 60) > 5);
        assert_eq!(count(&f, OK, 0, 0, 320, 170), 0);
        let fresh = Store::new();
        set(&fresh, "v", Value::Number(42.0));
        let stale = Store::new();
        stale.set_at("v", Value::Number(42.0), now() - Duration::seconds(900));
        let ff = draw(vec![kpi("v", Some("ok"), Thresholds::default(), None)], &fresh);
        let fs = draw(vec![kpi("v", Some("ok"), Thresholds::default(), None)], &stale);
        assert!(count(&ff, OK, 0, 20, 160, 60) > 20);
        assert_eq!(count(&fs, OK, 0, 0, 320, 170), 0);
        assert!(count(&fs, MUTED, 0, 20, 160, 60) > 20);
        let (_, right_fresh) = lit_cols(&ff, 20, 60).unwrap();
        let (_, right_stale) = lit_cols(&fs, 20, 60).unwrap();
        assert!(right_stale > right_fresh + 8, "age suffix: {right_stale} vs {right_fresh}");
    }

    #[test]
    fn kpi_sizes_change_the_number_height() {
        let s = Store::new();
        set(&s, "v", Value::Number(8.0));
        let rows = |f: &Frame| (0..170).filter(|&y| count(f, TEXT, 0, y, 160, y + 1) > 0).count();
        let mid = draw(vec![kpi("v", None, Thresholds::default(), Some("mid"))], &s);
        let big = draw(vec![kpi("v", None, Thresholds::default(), None)], &s);
        let clock = draw(vec![kpi("v", None, Thresholds::default(), Some("clock"))], &s);
        assert!(rows(&mid) < rows(&big) && rows(&big) < rows(&clock), "{} {} {}", rows(&mid), rows(&big), rows(&clock));
    }

    fn gauge(v: Option<f64>) -> (Frame, Store) {
        let s = Store::new();
        if let Some(v) = v {
            set(&s, "g", Value::Number(v));
        }
        let w = Widget::Gauge {
            x: 100,
            y: 80,
            r: 30,
            bind: "g".into(),
            label: Some("cpu".into()),
            color: Some(Color("ok".into())),
            thresholds: Thresholds::default(),
        };
        (draw(vec![w], &s), s)
    }

    #[test]
    fn gauge_fills_the_semicircle_from_the_left() {
        let (f, _) = gauge(Some(0.3));
        assert_eq!(px(&f, 72, 78), rgb(OK));
        assert_eq!(px(&f, 128, 78), rgb(EDGE));
        assert_eq!(px(&f, 100, 52), rgb(EDGE));
        assert_eq!(px(&f, 100, 108), rgb(CARD));
        assert!(lit_cols(&f, 70, 80).is_some(), "value text inside");
        assert!(lit_cols(&f, 86, 100).is_some(), "label under");
        let (full, _) = gauge(Some(1.7));
        assert_eq!(px(&full, 128, 78), rgb(OK));
        let (none, _) = gauge(None);
        assert_eq!(px(&none, 72, 78), rgb(EDGE));
    }

    #[test]
    fn hbar_fills_proportionally_with_value_on_the_right() {
        let s = Store::new();
        set(&s, "h", Value::Number(0.5));
        let w = Widget::Hbar {
            x: 10,
            y: 10,
            w: 300,
            bind: "h".into(),
            label: Some("mem".into()),
            color: Some(Color("ok".into())),
            thresholds: Thresholds::default(),
        };
        let f = draw(vec![w], &s);
        let fill = count(&f, OK, 0, 10, 320, 22);
        let track = count(&f, EDGE, 0, 10, 320, 22);
        assert!(fill > 100 && track > 100, "fill {fill} track {track}");
        assert!(fill.abs_diff(track) < fill / 4, "about half: {fill} vs {track}");
        let (_, right) = lit_cols(&f, 10, 22).unwrap();
        assert!(right > 300 && right <= 310, "value text at right edge: {right}");
    }

    fn spark(series: Option<Vec<f64>>) -> Frame {
        let s = Store::new();
        if let Some(v) = series {
            set(&s, "sp", Value::Series(v));
        }
        draw(
            vec![Widget::Spark { x: 20, y: 20, w: 200, h: 100, bind: "sp".into(), color: Some(Color("ok".into())) }],
            &s,
        )
    }

    #[test]
    fn spark_draws_line_area_and_end_dot() {
        let f = spark(Some(vec![0.0, 1.0, 2.0, 3.0]));
        assert!(count(&f, OK, 214, 18, 222, 26) > 6, "end dot top right");
        let area = mix(OK, CARD, 0.25);
        let near =
            |p: Rgb| (p.0 as i32 - area.red() as i32).abs() < 12 && (p.1 as i32 - area.green() as i32).abs() < 12;
        assert!(near(px(&f, 200, 110)), "area fill under the line: {:?}", px(&f, 200, 110));
        assert_eq!(px(&f, 30, 30), rgb(CARD), "nothing above the line");
    }

    #[test]
    fn flat_spark_sits_in_the_middle_and_empty_is_a_dashed_baseline() {
        let f = spark(Some(vec![5.0, 5.0, 5.0]));
        assert!(count(&f, OK, 30, 68, 200, 72) > 100);
        let e = spark(None);
        assert_eq!(count(&e, OK, 0, 0, 320, 170), 0);
        let base = count(&e, MUTED, 20, 119, 220, 120);
        assert!(base > 60 && base < 140, "dashed: {base}");
        let one = spark(Some(vec![7.0]));
        assert!(count(&one, OK, 0, 0, 320, 170) > 0);
    }

    #[test]
    fn bars_scale_to_max_and_brighten_the_last() {
        let s = Store::new();
        set(&s, "b", Value::Series(vec![1.0, 2.0, 3.0, 4.0]));
        let f = draw(
            vec![Widget::Bars { x: 10, y: 10, w: 79, h: 100, bind: "b".into(), color: Some(Color("ok".into())) }],
            &s,
        );
        let dim = mix(OK, CARD, 0.6);
        assert_eq!(px(&f, 15, 105), rgb(dim));
        assert_eq!(px(&f, 15, 70), rgb(CARD), "first bar is a quarter high");
        assert_eq!(px(&f, 80, 12), rgb(OK), "last bar full height");
        assert_eq!(px(&f, 29, 105), rgb(CARD), "1 px gap");
        let e = draw(vec![Widget::Bars { x: 10, y: 10, w: 79, h: 100, bind: "x".into(), color: None }], &Store::new());
        assert!(count(&e, MUTED, 10, 109, 89, 110) > 10);
    }

    fn text(t: &str, align: Option<&str>, color: Option<&str>) -> Widget {
        Widget::Text {
            x: 160,
            y: 50,
            text: t.into(),
            font: Some("mid".into()),
            color: color.map(|c| Color(c.into())),
            align: align.map(Into::into),
        }
    }

    #[test]
    fn text_substitutes_and_aligns() {
        let s = Store::new();
        set(&s, "t", Value::Number(0.42));
        let f = draw(vec![text("used {{t|pct}}", Some("right"), Some("ok"))], &s);
        let (l, r) = lit_cols(&f, 45, 75).unwrap();
        assert!(r <= 160 && l < 120, "{l}..{r}");
        assert!(count(&f, OK, 0, 45, 320, 75) > 10);
        let f = draw(vec![text("used {{t|pct}}", None, Some("ok"))], &s);
        assert!(lit_cols(&f, 45, 75).unwrap().0 >= 160);
        let f = draw(vec![text("used {{missing}}", None, Some("ok"))], &s);
        assert_eq!(count(&f, OK, 0, 0, 320, 170), 0);
        assert!(count(&f, MUTED, 0, 45, 320, 75) > 10);
    }

    #[test]
    fn rows_draw_name_led_num_bar_spark_per_item() {
        let s = Store::new();
        set(&s, "up/a", Value::Number(1.0));
        set(&s, "up/b", Value::Number(0.0));
        set(&s, "mem/a", Value::Number(0.95));
        set(&s, "mem/b", Value::Number(0.2));
        set(&s, "sp/a", Value::Series(vec![1.0, 3.0, 2.0]));
        let items = ["a", "b", "c"].iter().map(|k| RowItem { key: (*k).into(), name: None }).collect();
        let th = Thresholds { warn: Some(0.8), bad: Some(0.9), invert: false };
        let cols = vec![
            RowCol::Led { bind: "up/{}".into(), thresholds: Thresholds::default() },
            RowCol::Name { w: 60 },
            RowCol::Num { bind: "mem/{}".into(), w: 40, fmt: Fmt::Pct, unit: None, thresholds: th },
            RowCol::Bar { bind: "mem/{}".into(), w: 60, color: None, thresholds: th },
            RowCol::Spark { bind: "sp/{}".into(), w: 50, color: Some(Color("ok".into())) },
        ];
        let f = draw(vec![Widget::Rows { x: 10, y: 10, w: 300, row_h: Some(20), items, cols }], &s);
        assert_eq!(px(&f, 13, 20), rgb(OK), "row a led");
        assert_eq!(px(&f, 13, 40), rgb(BAD), "row b led");
        assert_eq!(px(&f, 13, 60), rgb(MUTED), "row c led missing");
        assert!(count(&f, BAD, 82, 10, 130, 30) > 5, "row a num in bad colour");
        assert!(count(&f, BAD, 130, 10, 192, 30) > 30, "row a bar bad");
        assert!(count(&f, OK, 130, 30, 192, 50) > 5, "row b bar ok");
        assert!(count(&f, OK, 192, 10, 245, 30) > 5, "row a spark");
        assert!(count(&f, MUTED, 82, 50, 130, 70) > 3, "row c num --");
    }

    #[test]
    fn row_names_are_clipped_to_their_column() {
        let items = vec![RowItem { key: "k".into(), name: Some("a-very-long-cluster-name".into()) }];
        let f = draw(
            vec![Widget::Rows { x: 10, y: 10, w: 300, row_h: None, items, cols: vec![RowCol::Name { w: 40 }] }],
            &Store::new(),
        );
        let (l, r) = lit_cols(&f, 10, 30).unwrap();
        assert!(l >= 10 && r < 50, "{l}..{r}");
    }

    #[test]
    fn dots_grid_by_state() {
        let s = Store::new();
        set(&s, "p/a", Value::Number(1.0));
        set(&s, "p/b", Value::Number(0.0));
        let items = ["a", "b", "c", "d"].iter().map(|k| RowItem { key: (*k).into(), name: None }).collect();
        let w =
            Widget::Dots { x: 10, y: 10, per_row: Some(2), items, bind: "p/{}".into(), label: Some("probes".into()) };
        let f = draw(vec![w], &s);
        assert_eq!(px(&f, 13, 27), rgb(OK));
        assert_eq!(px(&f, 23, 27), rgb(BAD));
        assert_eq!(px(&f, 13, 37), rgb(MUTED));
        assert_eq!(px(&f, 23, 37), rgb(MUTED));
        assert!(lit_cols(&f, 10, 22).is_some(), "label");
    }

    fn ticker_at(msg: &str, t: i64) -> Frame {
        let mut c = ctx();
        c.now = Utc.timestamp_opt(t, 0).unwrap();
        c.message = msg.into();
        let p = page(vec![Widget::Ticker { x: 20, y: 60, w: 100, bind: None, color: None }]);
        render_page(&p, &Store::new(), &c, 320, 170)
    }

    #[test]
    fn ticker_scrolls_long_text_inside_its_box() {
        let long = "a very long message that does not fit in one hundred pixels";
        let a = ticker_at(long, 1_000);
        let b = ticker_at(long, 1_001);
        assert_ne!(a, b);
        let (l, r) = lit_cols(&a, 55, 85).unwrap();
        assert!(l >= 20 && r < 120, "clipped to box: {l}..{r}");
        assert_eq!(ticker_at("short", 1_000), ticker_at("short", 1_001));
        assert!(lit_cols(&ticker_at("", 1_000), 55, 85).is_none());
    }

    #[test]
    fn ticker_prefers_the_bound_text() {
        let s = Store::new();
        set(&s, "msg", Value::Text("hi".into()));
        let f = draw(
            vec![Widget::Ticker { x: 20, y: 60, w: 100, bind: Some("msg".into()), color: Some(Color("ok".into())) }],
            &s,
        );
        assert!(count(&f, OK, 0, 55, 320, 85) > 5);
    }

    #[test]
    fn image_fits_keeping_aspect() {
        let s = Store::new();
        let red = [255u8, 0, 0, 255].repeat(4 * 2);
        set(&s, "cam", Value::Image { width: 4, height: 2, rgba: std::sync::Arc::new(red) });
        let f = draw(vec![Widget::Image { x: 10, y: 10, w: 80, h: 80, bind: "cam".into(), label: None }], &s);
        assert_eq!(px(&f, 50, 50), (255, 0, 0));
        assert_ne!(px(&f, 50, 20), (255, 0, 0), "letterboxed");
        let e =
            draw(vec![Widget::Image { x: 10, y: 10, w: 80, h: 80, bind: "none".into(), label: None }], &Store::new());
        assert!(near(&e, MUTED, 11, 11, 89, 89) > 5, "no image placeholder");
        assert!(count(&e, EDGE, 10, 10, 90, 90) > 100, "border");
    }

    #[test]
    fn degenerate_images_draw_the_placeholder_without_panicking() {
        for (w, h, len) in [(0u32, 0u32, 0usize), (4, 2, 5), (0, 3, 0)] {
            let s = Store::new();
            set(&s, "cam", Value::Image { width: w, height: h, rgba: std::sync::Arc::new(vec![9; len]) });
            let f = draw(vec![Widget::Image { x: 10, y: 10, w: 80, h: 80, bind: "cam".into(), label: None }], &s);
            assert!(near(&f, MUTED, 11, 11, 89, 89) > 5, "{w}x{h} len {len}");
        }
    }

    #[test]
    fn header_shows_title_sources_and_index() {
        let mut p = page(vec![]);
        p.header = true;
        p.title = Some("Clusters".into());
        p.cards[0].y = 22;
        p.cards[0].h = 148;
        let mut c = ctx();
        c.sources = vec![("a".into(), true), ("b".into(), false)];
        let f = render_page(&p, &Store::new(), &c, 320, 170);
        assert!(lit_cols(&f, 0, 18).unwrap().0 < 12, "title at left");
        assert!(count(&f, OK, 240, 0, 320, 18) > 3);
        assert!(count(&f, BAD, 240, 0, 320, 18) > 3);
        assert!(lit_cols(&f, 0, 18).unwrap().1 > 305, "page index at right");
        assert_eq!(px(&f, 160, 18), rgb(EDGE));
    }

    #[test]
    fn card_title_and_accent() {
        let mut p = page(vec![]);
        p.cards[0] = Card {
            x: 10,
            y: 10,
            w: 200,
            h: 100,
            accent: Some(Color("bad".into())),
            title: Some("Title".into()),
            widgets: vec![],
        };
        let f = render_page(&p, &Store::new(), &ctx(), 320, 170);
        assert_eq!(px(&f, 12, 60), rgb(BAD));
        let bright = |x0, x1| {
            (12..26).flat_map(|y| (x0..x1).map(move |x| (x, y))).filter(|&(x, y)| px(&f, x, y).1 > 0x50).count()
        };
        assert!(bright(16, 80) > 20, "title text");
        assert_eq!(bright(14, 16), 0);
    }

    #[test]
    fn header_uses_the_design_width_on_a_scaled_canvas() {
        let mut p = page(vec![]);
        p.header = true;
        p.cards[0].y = 22;
        p.cards[0].h = 148;
        let mut c = ctx();
        c.sources = vec![("a".into(), true), ("b".into(), false)];
        let store = Store::new();
        let mut cv = crate::canvas::Canvas::new_scaled(640, 340, 2.0);
        let look = crate::values::Lookup { store: &store, now: c.now, stale_secs: c.stale_secs };
        super::Painter { cv: &mut cv, look, ctx: &c }.page(&p);
        let f = Frame { width: 640, height: 340, rgba: cv.pixmap.take() };
        assert!(count(&f, OK, 480, 0, 640, 36) > 12, "source dots near the right edge");
        assert!(count(&f, BAD, 480, 0, 640, 36) > 12);
        assert_eq!(px(&f, 320, 36), rgb(EDGE));
    }
}
