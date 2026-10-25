use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};

/// One value a plugin produced. Keys are `plugin/key` or `plugin/key/label`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Text(String),
    Series(Vec<f64>),
    /// Decoded RGBA8 pixels, row-major.
    Image {
        width: u32,
        height: u32,
        rgba: Arc<Vec<u8>>,
    },
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }
    pub fn as_series(&self) -> Option<&[f64]> {
        match self {
            Value::Series(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub value: Value,
    pub updated_at: DateTime<Utc>,
}

impl Stamped {
    pub fn age_secs(&self, now: DateTime<Utc>) -> i64 {
        (now - self.updated_at).num_seconds().max(0)
    }
}

/// Latest value per key, shared between the scheduler (writer) and the renderer (reader).
#[derive(Debug, Default, Clone)]
pub struct Store {
    inner: Arc<RwLock<BTreeMap<String, Stamped>>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, key: impl Into<String>, value: Value) {
        self.set_at(key, value, Utc::now());
    }

    pub fn set_at(&self, key: impl Into<String>, value: Value, at: DateTime<Utc>) {
        let mut map = self.inner.write().expect("store poisoned");
        map.insert(key.into(), Stamped { value, updated_at: at });
    }

    pub fn set_many(&self, prefix: &str, values: Vec<(String, Value)>) {
        let now = Utc::now();
        let mut map = self.inner.write().expect("store poisoned");
        for (key, value) in values {
            map.insert(format!("{prefix}/{key}"), Stamped { value, updated_at: now });
        }
    }

    pub fn get(&self, key: &str) -> Option<Stamped> {
        self.inner.read().expect("store poisoned").get(key).cloned()
    }

    pub fn num(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|s| s.value.as_f64())
    }

    pub fn text(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|s| s.value.as_text().map(str::to_owned))
    }

    pub fn series(&self, key: &str) -> Option<Vec<f64>> {
        self.get(key).and_then(|s| s.value.as_series().map(<[f64]>::to_vec))
    }

    /// Keys under `prefix/` with the remainder of the key, sorted.
    pub fn children(&self, prefix: &str) -> Vec<(String, Stamped)> {
        let want = format!("{prefix}/");
        self.inner
            .read()
            .expect("store poisoned")
            .range(want.clone()..)
            .take_while(|(k, _)| k.starts_with(&want))
            .map(|(k, v)| (k[want.len()..].to_owned(), v.clone()))
            .collect()
    }

    pub fn snapshot(&self) -> BTreeMap<String, Stamped> {
        self.inner.read().expect("store poisoned").clone()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("store poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_many_prefixes_keys_and_get_reads_them_back() {
        let store = Store::new();
        store.set_many("prom", vec![("cpu".into(), Value::Number(0.5)), ("name".into(), Value::Text("a".into()))]);
        assert_eq!(store.num("prom/cpu"), Some(0.5));
        assert_eq!(store.text("prom/name").as_deref(), Some("a"));
        assert_eq!(store.num("prom/missing"), None);
    }

    #[test]
    fn children_returns_only_the_prefix_subtree_sorted() {
        let store = Store::new();
        store.set("h/cpu/b", Value::Number(2.0));
        store.set("h/cpu/a", Value::Number(1.0));
        store.set("h/cpuX", Value::Number(9.0));
        store.set("h/mem/a", Value::Number(3.0));
        let got: Vec<String> = store.children("h/cpu").into_iter().map(|(k, _)| k).collect();
        assert_eq!(got, vec!["a", "b"]);
    }

    #[test]
    fn age_is_measured_from_the_stamp() {
        let store = Store::new();
        let t0 = Utc::now() - chrono::Duration::seconds(90);
        store.set_at("k", Value::Number(1.0), t0);
        let s = store.get("k").unwrap();
        assert!(s.age_secs(Utc::now()) >= 90);
    }
}
