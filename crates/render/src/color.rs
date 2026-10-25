use peephole_core::Color;
use tiny_skia::ColorU8;

const fn hex(v: u32) -> ColorU8 {
    ColorU8::from_rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, 255)
}

pub const BG: ColorU8 = hex(0x0B0E14);
pub const CARD: ColorU8 = hex(0x151A23);
pub const EDGE: ColorU8 = hex(0x232A36);
pub const TEXT: ColorU8 = hex(0xE6E9EF);
pub const TEXT2: ColorU8 = hex(0x8A93A5);
pub const MUTED: ColorU8 = hex(0x475062);
pub const OK: ColorU8 = hex(0x3DDC84);
pub const WARN: ColorU8 = hex(0xF5C518);
pub const BAD: ColorU8 = hex(0xFF4D4D);

const TOKENS: &[(&str, ColorU8)] = &[
    ("bg", BG),
    ("card", CARD),
    ("edge", EDGE),
    ("text", TEXT),
    ("text2", TEXT2),
    ("muted", MUTED),
    ("bitcoin", hex(0xF7931A)),
    ("web", hex(0x4EA1FF)),
    ("ai", hex(0xA78BFA)),
    ("net", hex(0x22D3EE)),
    ("ok", OK),
    ("warn", WARN),
    ("bad", BAD),
];

/// A design token name or `#RRGGBB`; anything else resolves to `text`.
pub fn resolve(c: &Color) -> ColorU8 {
    let name = c.0.trim();
    if let Some(h) = name.strip_prefix('#') {
        return match (h.len(), u32::from_str_radix(h, 16)) {
            (6, Ok(v)) => hex(v),
            _ => TEXT,
        };
    }
    TOKENS.iter().find(|(n, _)| *n == name).map_or(TEXT, |(_, c)| *c)
}

/// 0 ok, 1 warn, anything else bad.
pub fn state(s: u8) -> ColorU8 {
    match s {
        0 => OK,
        1 => WARN,
        _ => BAD,
    }
}

/// `fg` at weight `t` over `bg`, opaque.
pub fn mix(fg: ColorU8, bg: ColorU8, t: f32) -> ColorU8 {
    let ch = |a: u8, b: u8| (a as f32 * t + b as f32 * (1.0 - t)).round() as u8;
    ColorU8::from_rgba(ch(fg.red(), bg.red()), ch(fg.green(), bg.green()), ch(fg.blue(), bg.blue()), 255)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(c: ColorU8) -> (u8, u8, u8) {
        (c.red(), c.green(), c.blue())
    }

    #[test]
    fn resolves_tokens_hex_and_unknown_names() {
        assert_eq!(rgb(resolve(&Color("bitcoin".into()))), (0xF7, 0x93, 0x1A));
        assert_eq!(rgb(resolve(&Color("ok".into()))), (0x3D, 0xDC, 0x84));
        assert_eq!(rgb(resolve(&Color("#102030".into()))), (0x10, 0x20, 0x30));
        assert_eq!(rgb(resolve(&Color("nope".into()))), rgb(TEXT));
        assert_eq!(rgb(resolve(&Color("#12".into()))), rgb(TEXT));
        assert_eq!(rgb(resolve(&Color("#GGGGGG".into()))), rgb(TEXT));
    }

    #[test]
    fn state_colours_follow_ok_warn_bad() {
        assert_eq!(rgb(state(0)), rgb(OK));
        assert_eq!(rgb(state(1)), rgb(WARN));
        assert_eq!(rgb(state(2)), rgb(BAD));
    }

    #[test]
    fn mix_blends_towards_the_background() {
        let m = mix(ColorU8::from_rgba(200, 100, 0, 255), ColorU8::from_rgba(0, 0, 100, 255), 0.5);
        assert_eq!(rgb(m), (100, 50, 50));
    }
}
