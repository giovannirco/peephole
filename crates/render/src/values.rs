use std::sync::Arc;

use chrono::{DateTime, Utc};
use peephole_core::{Fmt, Stamped, Store, Value};

use crate::format::{format_age, format_value, parse_fmt};

/// A store value as the renderer sees it: missing, fresh, or stale with its age in seconds.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading<T> {
    Missing,
    Fresh(T),
    Stale(T, i64),
}

impl<T> Reading<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Reading::Missing => None,
            Reading::Fresh(v) | Reading::Stale(v, _) => Some(v),
        }
    }

    /// Missing or stale: drawn in `muted`.
    pub fn is_dim(&self) -> bool {
        !matches!(self, Reading::Fresh(_))
    }
}

/// An RGBA8 image value.
pub struct Img {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

pub struct Lookup<'a> {
    pub store: &'a Store,
    pub now: DateTime<Utc>,
    pub stale_secs: u64,
}

impl Lookup<'_> {
    fn read<T>(&self, key: &str, pick: impl FnOnce(Value) -> Option<T>) -> Reading<T> {
        let Some(Stamped { value, updated_at }) = self.store.get(key) else { return Reading::Missing };
        let age = (self.now - updated_at).num_seconds().max(0);
        match pick(value) {
            None => Reading::Missing,
            Some(v) if age as u64 > self.stale_secs => Reading::Stale(v, age),
            Some(v) => Reading::Fresh(v),
        }
    }

    pub fn num(&self, key: &str) -> Reading<f64> {
        self.read(key, |v| v.as_f64())
    }

    pub fn text(&self, key: &str) -> Reading<String> {
        self.read(key, |v| match v {
            Value::Text(t) => Some(t),
            _ => None,
        })
    }

    pub fn series(&self, key: &str) -> Reading<Vec<f64>> {
        self.read(key, |v| match v {
            Value::Series(s) => Some(s),
            _ => None,
        })
    }

    pub fn image(&self, key: &str) -> Reading<Img> {
        self.read(key, |v| match v {
            Value::Image { width, height, rgba } => Some(Img { width, height, rgba }),
            _ => None,
        })
    }

    fn age(&self, key: &str) -> Reading<i64> {
        let Some(s) = self.store.get(key) else { return Reading::Missing };
        let age = s.age_secs(self.now);
        if age as u64 > self.stale_secs { Reading::Stale(age, age) } else { Reading::Fresh(age) }
    }

    /// Replace `{{key}}` / `{{key|fmt}}` / `{{key|age}}` from the store. Returns the text and
    /// whether any placeholder was missing, stale or had an unknown format (kept verbatim).
    pub fn expand(&self, template: &str) -> (String, bool) {
        let mut out = String::with_capacity(template.len());
        let mut dim = false;
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            let Some(len) = rest[start + 2..].find("}}") else { break };
            out.push_str(&rest[..start]);
            let inner = &rest[start + 2..start + 2 + len];
            let (key, fmt) = match inner.split_once('|') {
                Some((k, f)) => (k.trim(), Some(f.trim())),
                None => (inner.trim(), None),
            };
            let (text, d) = self.placeholder(key, fmt).unwrap_or_else(|| (format!("{{{{{inner}}}}}"), true));
            out.push_str(&text);
            dim |= d;
            rest = &rest[start + 2 + len + 2..];
        }
        out.push_str(rest);
        (out, dim)
    }

    fn placeholder(&self, key: &str, fmt: Option<&str>) -> Option<(String, bool)> {
        let shown = |r: Reading<String>| {
            let dim = r.is_dim();
            (r.value().unwrap_or_else(|| "--".into()), dim)
        };
        if fmt == Some("age") {
            return Some(shown(match self.age(key) {
                Reading::Missing => Reading::Missing,
                Reading::Fresh(a) => Reading::Fresh(format_age(a)),
                Reading::Stale(a, s) => Reading::Stale(format_age(a), s),
            }));
        }
        let f = match fmt {
            Some(name) => parse_fmt(name)?,
            None => Fmt::Auto,
        };
        if let r @ (Reading::Fresh(_) | Reading::Stale(..)) = self.text(key) {
            return Some(shown(r));
        }
        Some(shown(match self.num(key) {
            Reading::Missing => Reading::Missing,
            Reading::Fresh(v) => Reading::Fresh(format_value(v, f)),
            Reading::Stale(v, a) => Reading::Stale(format_value(v, f), a),
        }))
    }
}

/// `{}` in a bind replaced by the item key.
pub fn bind_item(bind: &str, key: &str) -> String {
    bind.replace("{}", key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use peephole_core::Value;

    fn setup() -> (Store, DateTime<Utc>) {
        let now = Utc::now();
        let s = Store::new();
        s.set_at("a/num", Value::Number(0.87), now);
        s.set_at("a/old", Value::Number(5.0), now - Duration::seconds(600));
        s.set_at("a/txt", Value::Text("hello".into()), now - Duration::seconds(180));
        s.set_at("a/ser", Value::Series(vec![1.0, 2.0]), now);
        (s, now)
    }

    #[test]
    fn readings_are_fresh_stale_or_missing() {
        let (s, now) = setup();
        let l = Lookup { store: &s, now, stale_secs: 300 };
        assert_eq!(l.num("a/num"), Reading::Fresh(0.87));
        assert_eq!(l.num("a/old"), Reading::Stale(5.0, 600));
        assert_eq!(l.num("a/none"), Reading::Missing);
        assert_eq!(l.num("a/txt"), Reading::Missing);
        assert_eq!(l.series("a/ser"), Reading::Fresh(vec![1.0, 2.0]));
        assert_eq!(l.text("a/txt"), Reading::Fresh("hello".into()));
        assert_eq!(l.num("a/old").value(), Some(5.0));
        assert!(l.num("a/old").is_dim() && l.num("a/x").is_dim() && !l.num("a/num").is_dim());
    }

    #[test]
    fn templates_replace_placeholders() {
        let (s, now) = setup();
        let l = Lookup { store: &s, now, stale_secs: 300 };
        assert_eq!(l.expand("cpu {{a/num|pct}} ok"), ("cpu 87% ok".into(), false));
        assert_eq!(l.expand("{{a/txt}} / {{a/txt|age}}"), ("hello / 3m".into(), false));
        assert_eq!(l.expand("{{ a/num }}"), ("0.87".into(), false));
        assert_eq!(l.expand("x {{a/none}} y"), ("x -- y".into(), true));
        assert_eq!(l.expand("{{a/old}}"), ("5".into(), true));
        assert_eq!(l.expand("{{a/ser}}"), ("--".into(), true));
        assert_eq!(l.expand("{{a/none|age}}"), ("--".into(), true));
        assert_eq!(l.expand("plain"), ("plain".into(), false));
        assert_eq!(l.expand("open {{a/num"), ("open {{a/num".into(), false));
        assert_eq!(l.expand("{{a/num|nope}}"), ("{{a/num|nope}}".into(), true));
    }

    #[test]
    fn bind_items_substitute_braces() {
        assert_eq!(bind_item("k/ready/{}", "web"), "k/ready/web");
        assert_eq!(bind_item("k/total", "web"), "k/total");
    }
}
