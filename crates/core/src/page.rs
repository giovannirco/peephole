use serde::{Deserialize, Serialize};

/// A page is a screen full of cards. Coordinates are in the design space
/// (`Page::width` × `Page::height`, default 320 × 170); the renderer scales.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Page {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    /// Draw the header strip (title left, data dots and page index right).
    #[serde(default = "default_true")]
    pub header: bool,
    #[serde(default)]
    pub cards: Vec<Card>,
}

fn default_width() -> u32 {
    320
}
fn default_height() -> u32 {
    170
}
fn default_true() -> bool {
    true
}

/// A rounded rectangle with a 3 px accent bar on the left; widgets use coordinates relative to the card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Card {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default)]
    pub accent: Option<Color>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub widgets: Vec<Widget>,
}

/// A colour token from the design system (`bitcoin`, `ai`, `ok`, …) or `#RRGGBB`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct Color(pub String);

/// How a number is printed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Fmt {
    /// `1234` → `1.2k`, `0.5` → `0.5`
    #[default]
    Auto,
    Int,
    /// ratio 0–1 → `87%`
    Pct,
    /// fraction with one decimal
    Dec1,
    /// `1536` → `1.5 KB`
    Bytes,
    /// seconds → `3d 4h`, `12m`
    Duration,
    /// `73°`
    Temp,
    /// `97,213`
    Thousands,
}

/// Colour by value: `bad` when past `bad`, `warn` when past `warn`, else `ok`.
/// `invert: true` means lower is worse (free space, quota left).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
pub struct Thresholds {
    #[serde(default)]
    pub warn: Option<f64>,
    #[serde(default)]
    pub bad: Option<f64>,
    #[serde(default)]
    pub invert: bool,
}

impl Thresholds {
    /// `Some(state)` where state is 0 ok, 1 warn, 2 bad; `None` when no thresholds are set.
    pub fn state(&self, v: f64) -> Option<u8> {
        if self.warn.is_none() && self.bad.is_none() {
            return None;
        }
        let past = |t: Option<f64>| t.is_some_and(|t| if self.invert { v < t } else { v > t });
        Some(if past(self.bad) {
            2
        } else if past(self.warn) {
            1
        } else {
            0
        })
    }
}

/// Widgets bind to store keys. `bind` is a full key (`prom/claude5h`); in `rows`
/// and `dots`, `{}` in a bind is replaced by each item label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Widget {
    /// Small label above a big number.
    Kpi {
        x: i32,
        y: i32,
        bind: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        unit: Option<String>,
        #[serde(default)]
        fmt: Fmt,
        #[serde(default)]
        color: Option<Color>,
        #[serde(default)]
        thresholds: Thresholds,
        /// `big` (26 px), `mid` (15 px), `clock` (44 px)
        #[serde(default)]
        size: Option<String>,
    },
    /// Semicircle gauge, ratio 0–1, value inside, label under.
    Gauge {
        x: i32,
        y: i32,
        r: i32,
        bind: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        color: Option<Color>,
        #[serde(default)]
        thresholds: Thresholds,
    },
    /// Horizontal bar, ratio 0–1, with label left and value right.
    Hbar {
        x: i32,
        y: i32,
        w: i32,
        bind: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        color: Option<Color>,
        #[serde(default)]
        thresholds: Thresholds,
    },
    /// Sparkline of a Series value with a faint area fill and a dot on the last point.
    Spark {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        bind: String,
        #[serde(default)]
        color: Option<Color>,
    },
    /// Bar chart of a Series value, last bar brighter.
    Bars {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        bind: String,
        #[serde(default)]
        color: Option<Color>,
    },
    /// Text with `{{key}}` placeholders replaced by store values (`{{key|fmt}}` to format).
    Text {
        x: i32,
        y: i32,
        text: String,
        /// `clock`, `num`, `mid`, `small` (default `small`)
        #[serde(default)]
        font: Option<String>,
        #[serde(default)]
        color: Option<Color>,
        /// `left` (default), `center`, `right`
        #[serde(default)]
        align: Option<String>,
    },
    /// Rows of items (clusters, hosts): one line per label, columns bound with `{}`.
    Rows {
        x: i32,
        y: i32,
        w: i32,
        /// Row pitch in px (default 20).
        #[serde(default)]
        row_h: Option<i32>,
        items: Vec<RowItem>,
        cols: Vec<RowCol>,
    },
    /// Grid of status dots: one per item, bind with `{}`; 1 = ok, 0 = bad, missing = grey.
    Dots {
        x: i32,
        y: i32,
        /// Dots per row (default 8).
        #[serde(default)]
        per_row: Option<i32>,
        items: Vec<RowItem>,
        bind: String,
        #[serde(default)]
        label: Option<String>,
    },
    /// One-line message that scrolls when wider than the card (the device message by default).
    Ticker {
        x: i32,
        y: i32,
        w: i32,
        #[serde(default)]
        bind: Option<String>,
        #[serde(default)]
        color: Option<Color>,
    },
    /// An Image value (camera snapshot) fitted into the box.
    Image {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        bind: String,
        #[serde(default)]
        label: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RowItem {
    /// Label value substituted for `{}` in binds.
    pub key: String,
    /// Text shown; defaults to `key`.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowCol {
    /// The item's name.
    Name { w: i32 },
    /// Green/red dot from a 0/1 value (or thresholds).
    Led {
        bind: String,
        #[serde(default)]
        thresholds: Thresholds,
    },
    Num {
        bind: String,
        w: i32,
        #[serde(default)]
        fmt: Fmt,
        #[serde(default)]
        unit: Option<String>,
        #[serde(default)]
        thresholds: Thresholds,
    },
    Bar {
        bind: String,
        w: i32,
        #[serde(default)]
        color: Option<Color>,
        #[serde(default)]
        thresholds: Thresholds,
    },
    Spark {
        bind: String,
        w: i32,
        #[serde(default)]
        color: Option<Color>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_state_normal_and_inverted() {
        let t = Thresholds { warn: Some(0.8), bad: Some(0.9), invert: false };
        assert_eq!(t.state(0.5), Some(0));
        assert_eq!(t.state(0.85), Some(1));
        assert_eq!(t.state(0.95), Some(2));
        let q = Thresholds { warn: Some(0.4), bad: Some(0.15), invert: true };
        assert_eq!(q.state(0.5), Some(0));
        assert_eq!(q.state(0.3), Some(1));
        assert_eq!(q.state(0.1), Some(2));
        assert_eq!(Thresholds::default().state(1.0), None);
    }

    #[test]
    fn page_yaml_round_trips_widgets() {
        let yaml = r#"
name: Overview
cards:
  - {x: 6, y: 24, w: 150, h: 140, accent: bitcoin, widgets: [
      {kind: kpi, x: 10, y: 8, bind: mempool/price_usd, label: BTC, fmt: thousands},
      {kind: spark, x: 10, y: 60, w: 130, h: 40, bind: mempool/price_24h},
      {kind: rows, x: 0, y: 0, w: 100, items: [{key: a}], cols: [{kind: name, w: 40}, {kind: led, bind: "k/ready/{}"}]}]}
"#;
        let page: Page = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(page.width, 320);
        assert_eq!(page.cards[0].widgets.len(), 3);
        assert!(matches!(page.cards[0].widgets[0], Widget::Kpi { fmt: Fmt::Thousands, .. }));
        let back = serde_yaml::to_string(&page).unwrap();
        let again: Page = serde_yaml::from_str(&back).unwrap();
        assert_eq!(page, again);
    }
}
