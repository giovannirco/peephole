use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// A screen that fetches frames. `id` is whatever the firmware sends (its MAC).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Device {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Index into the configured pages.
    pub page: usize,
    /// Seconds per page when cycling; 0 = stay.
    pub cycle_s: u64,
    #[serde(default)]
    pub message: String,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// When the page was last changed (by a button, the API or the cycle).
    pub page_since: DateTime<Utc>,
    /// A plugin event showing `page` until `until`.
    #[serde(default)]
    pub hold: Option<Hold>,
    #[serde(default)]
    pub frames: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hold {
    pub page: String,
    pub until: DateTime<Utc>,
    pub reason: String,
}

/// All known devices; optionally persisted as JSON.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<BTreeMap<String, Device>>>,
    path: Option<PathBuf>,
    default_cycle_s: u64,
}

impl Registry {
    pub fn new(default_cycle_s: u64) -> Self {
        Self { default_cycle_s, ..Default::default() }
    }

    /// Load from `path` when it exists; save there after every change.
    pub fn with_file(mut self, path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let devices: BTreeMap<String, Device> = serde_json::from_str(&text)?;
            *self.inner.write().expect("registry poisoned") = devices;
        }
        self.path = Some(path);
        Ok(self)
    }

    /// Register or refresh a device at `now`; returns its current state.
    pub fn touch(&self, id: &str, width: u32, height: u32, now: DateTime<Utc>) -> Device {
        let mut map = self.inner.write().expect("registry poisoned");
        let is_new = !map.contains_key(id);
        let d = map.entry(id.to_owned()).or_insert_with(|| Device {
            id: id.to_owned(),
            name: None,
            width,
            height,
            page: 0,
            cycle_s: self.default_cycle_s,
            message: String::new(),
            first_seen: now,
            last_seen: now,
            page_since: now,
            hold: None,
            frames: 0,
        });
        d.width = width;
        d.height = height;
        d.last_seen = now;
        d.frames += 1;
        let out = d.clone();
        drop(map);
        if is_new {
            self.persist();
        }
        out
    }

    pub fn get(&self, id: &str) -> Option<Device> {
        self.inner.read().expect("registry poisoned").get(id).cloned()
    }

    pub fn list(&self) -> Vec<Device> {
        self.inner.read().expect("registry poisoned").values().cloned().collect()
    }

    pub fn update(&self, id: &str, f: impl FnOnce(&mut Device)) -> Option<Device> {
        let out = {
            let mut map = self.inner.write().expect("registry poisoned");
            let d = map.get_mut(id)?;
            f(d);
            Some(d.clone())
        };
        self.persist();
        out
    }

    /// Move `delta` pages (wrapping) and reset the cycle timer; clears any hold.
    pub fn step(&self, id: &str, delta: i64, page_count: usize, now: DateTime<Utc>) -> Option<Device> {
        if page_count == 0 {
            return self.get(id);
        }
        self.update(id, |d| {
            let n = page_count as i64;
            d.page = (((d.page as i64 + delta) % n + n) % n) as usize;
            d.page_since = now;
            d.hold = None;
        })
    }

    pub fn set_page(&self, id: &str, page: usize, now: DateTime<Utc>) -> Option<Device> {
        self.update(id, |d| {
            d.page = page;
            d.page_since = now;
            d.hold = None;
        })
    }

    pub fn set_message(&self, id: &str, message: &str) -> Option<Device> {
        self.update(id, |d| d.message = message.to_owned())
    }

    /// Apply a plugin event to every device (an event is for the whole house).
    pub fn hold_all(&self, page: &str, hold: std::time::Duration, reason: &str, now: DateTime<Utc>) {
        let until = now + Duration::from_std(hold).unwrap_or_else(|_| Duration::seconds(20));
        {
            let mut map = self.inner.write().expect("registry poisoned");
            for d in map.values_mut() {
                d.hold = Some(Hold { page: page.to_owned(), until, reason: reason.to_owned() });
            }
        }
        self.persist();
    }

    /// The page index to draw now: a live hold wins; otherwise the cycle may have advanced.
    /// Advancing is written back so buttons continue from what is on screen.
    pub fn current_page(&self, id: &str, page_names: &[String], now: DateTime<Utc>) -> Option<usize> {
        let d = self.get(id)?;
        if page_names.is_empty() {
            return Some(0);
        }
        if let Some(h) = d.hold.as_ref().filter(|h| h.until > now)
            && let Some(i) = page_names.iter().position(|n| n == &h.page)
        {
            return Some(i);
        }
        let mut page = d.page.min(page_names.len() - 1);
        let mut since = d.page_since;
        let elapsed = (now - d.page_since).num_seconds().max(0) as u64;
        if let Some(steps) = elapsed.checked_div(d.cycle_s).filter(|s| *s > 0) {
            page = (page + steps as usize) % page_names.len();
            since = d.page_since + Duration::seconds((steps * d.cycle_s) as i64);
        }
        if page != d.page || since != d.page_since || d.hold.is_some() {
            self.update(id, |d| {
                d.page = page;
                d.page_since = since;
                if d.hold.as_ref().is_some_and(|h| h.until <= now) {
                    d.hold = None;
                }
            });
        }
        Some(page)
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let map = self.inner.read().expect("registry poisoned");
        match serde_json::to_string_pretty(&*map) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path, text) {
                    tracing::warn!(error = %e, path = %path.display(), "could not persist devices");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not serialise devices"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        ["a", "b", "c"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn touch_registers_once_and_counts_frames() {
        let r = Registry::new(10);
        let t = Utc::now();
        r.touch("m1", 320, 170, t);
        let d = r.touch("m1", 320, 170, t);
        assert_eq!(d.frames, 2);
        assert_eq!(r.list().len(), 1);
        assert_eq!(d.cycle_s, 10);
    }

    #[test]
    fn step_wraps_both_ways() {
        let r = Registry::new(0);
        let t = Utc::now();
        r.touch("m", 320, 170, t);
        assert_eq!(r.step("m", -1, 3, t).unwrap().page, 2);
        assert_eq!(r.step("m", 1, 3, t).unwrap().page, 0);
        assert_eq!(r.step("m", 4, 3, t).unwrap().page, 1);
    }

    #[test]
    fn cycle_advances_with_time_and_a_button_restarts_the_timer() {
        let r = Registry::new(10);
        let t0 = Utc::now();
        r.touch("m", 320, 170, t0);
        assert_eq!(r.current_page("m", &names(), t0 + Duration::seconds(5)), Some(0));
        assert_eq!(r.current_page("m", &names(), t0 + Duration::seconds(25)), Some(2));
        assert_eq!(r.current_page("m", &names(), t0 + Duration::seconds(32)), Some(0));
        let t1 = t0 + Duration::seconds(33);
        r.step("m", 1, 3, t1);
        assert_eq!(r.current_page("m", &names(), t1 + Duration::seconds(9)), Some(1));
        assert_eq!(r.current_page("m", &names(), t1 + Duration::seconds(11)), Some(2));
    }

    #[test]
    fn hold_shows_the_event_page_until_it_expires() {
        let r = Registry::new(0);
        let t0 = Utc::now();
        r.touch("m", 320, 170, t0);
        r.hold_all("c", std::time::Duration::from_secs(20), "ring", t0);
        assert_eq!(r.current_page("m", &names(), t0 + Duration::seconds(5)), Some(2));
        assert_eq!(r.current_page("m", &names(), t0 + Duration::seconds(25)), Some(0));
        assert!(r.get("m").unwrap().hold.is_none());
    }

    #[test]
    fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("peephole-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devices.json");
        let r = Registry::new(10).with_file(&path).unwrap();
        r.touch("m", 320, 170, Utc::now());
        r.set_message("m", "hello");
        let r2 = Registry::new(10).with_file(&path).unwrap();
        assert_eq!(r2.get("m").unwrap().message, "hello");
    }
}
