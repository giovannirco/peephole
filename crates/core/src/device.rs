use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    /// Page names this device steps and cycles through, in order; empty = every page.
    #[serde(default)]
    pub pages: Vec<String>,
}

impl Device {
    /// Global page indices this device steps and cycles through: its list, or every page.
    fn rotation(&self, page_names: &[String]) -> Vec<usize> {
        let listed: Vec<usize> =
            self.pages.iter().filter_map(|name| page_names.iter().position(|p| p == name)).collect();
        if listed.is_empty() { (0..page_names.len()).collect() } else { listed }
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum PageError {
    #[error("pages are not configured: {}", quoted(.0))]
    Unknown(Vec<String>),
    #[error("page {0:?} is listed twice")]
    Duplicate(String),
    #[error("page {0:?} is not in this device's page list")]
    NotInList(String),
}

fn quoted(names: &[String]) -> String {
    names.iter().map(|n| format!("{n:?}")).collect::<Vec<_>>().join(", ")
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
            pages: Vec::new(),
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

    /// Move `delta` pages (wrapping) within the device's rotation and reset the cycle timer; clears any hold.
    /// From a page outside the rotation it lands on the rotation's first page.
    pub fn step(&self, id: &str, delta: i64, page_names: &[String], now: DateTime<Utc>) -> Option<Device> {
        if page_names.is_empty() {
            return self.get(id);
        }
        self.update(id, |d| {
            let rotation = d.rotation(page_names);
            d.page = match rotation.iter().position(|i| *i == d.page) {
                Some(at) => rotation[(at as i64 + delta).rem_euclid(rotation.len() as i64) as usize],
                None => rotation[0],
            };
            d.page_since = now;
            d.hold = None;
        })
    }

    /// Show the page `name`; with a page list it must be one of the listed pages.
    pub fn set_page(
        &self,
        id: &str,
        name: &str,
        page_names: &[String],
        now: DateTime<Utc>,
    ) -> Result<Option<Device>, PageError> {
        let page =
            page_names.iter().position(|n| n == name).ok_or_else(|| PageError::Unknown(vec![name.to_owned()]))?;
        let Some(device) = self.get(id) else { return Ok(None) };
        if !device.pages.is_empty() && !device.pages.iter().any(|n| n == name) {
            return Err(PageError::NotInList(name.to_owned()));
        }
        Ok(self.update(id, |d| {
            d.page = page;
            d.page_since = now;
            d.hold = None;
        }))
    }

    /// Restrict the device to `pages` (in that order; empty = every page). When the current page is not
    /// in the list the device moves to its first entry.
    pub fn set_pages(
        &self,
        id: &str,
        pages: Vec<String>,
        page_names: &[String],
        now: DateTime<Utc>,
    ) -> Result<Option<Device>, PageError> {
        let unknown: Vec<String> = pages.iter().filter(|n| !page_names.contains(n)).cloned().collect();
        if !unknown.is_empty() {
            return Err(PageError::Unknown(unknown));
        }
        if let Some(twice) = pages.iter().enumerate().find(|(i, n)| pages[..*i].contains(n)).map(|(_, n)| n) {
            return Err(PageError::Duplicate(twice.clone()));
        }
        Ok(self.update(id, |d| {
            d.pages = pages;
            let rotation = d.rotation(page_names);
            if !rotation.contains(&d.page) {
                d.page = rotation[0];
                d.page_since = now;
            }
        }))
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
        let (page, since) = cycled(&d, &d.rotation(page_names), now);
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

/// Where the cycle has taken `d` by `now` within `rotation`, and when that page started.
/// A page outside the rotation starts over on its first page.
fn cycled(d: &Device, rotation: &[usize], now: DateTime<Utc>) -> (usize, DateTime<Utc>) {
    let Some(at) = rotation.iter().position(|i| *i == d.page) else { return (rotation[0], now) };
    let elapsed = (now - d.page_since).num_seconds().max(0) as u64;
    match elapsed.checked_div(d.cycle_s).filter(|s| *s > 0) {
        Some(steps) => (
            rotation[(at + steps as usize) % rotation.len()],
            d.page_since + Duration::seconds((steps * d.cycle_s) as i64),
        ),
        None => (d.page, d.page_since),
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
        assert_eq!(r.step("m", -1, &names(), t).unwrap().page, 2);
        assert_eq!(r.step("m", 1, &names(), t).unwrap().page, 0);
        assert_eq!(r.step("m", 4, &names(), t).unwrap().page, 1);
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
        r.step("m", 1, &names(), t1);
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

    fn four() -> Vec<String> {
        ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect()
    }

    fn listed(cycle_s: u64, list: &[&str], t: DateTime<Utc>) -> Registry {
        let r = Registry::new(cycle_s);
        r.touch("m", 320, 170, t);
        r.set_pages("m", list.iter().map(|s| s.to_string()).collect(), &four(), t).unwrap();
        r
    }

    #[test]
    fn setting_a_page_list_moves_to_its_first_page_when_the_current_one_is_not_in_it() {
        let t = Utc::now();
        let d = listed(0, &["b", "d"], t).get("m").unwrap();
        assert_eq!(d.pages, vec!["b", "d"]);
        assert_eq!(d.page, 1);
    }

    #[test]
    fn next_and_prev_wrap_inside_the_page_list() {
        let t = Utc::now();
        let r = listed(0, &["b", "d"], t);
        assert_eq!(r.step("m", 1, &four(), t).unwrap().page, 3);
        assert_eq!(r.step("m", 1, &four(), t).unwrap().page, 1);
        assert_eq!(r.step("m", -1, &four(), t).unwrap().page, 3);
        assert_eq!(r.step("m", -1, &four(), t).unwrap().page, 1);
    }

    #[test]
    fn stepping_from_a_page_outside_the_list_lands_on_its_first_entry() {
        let t = Utc::now();
        let r = listed(0, &["b", "d"], t);
        r.update("m", |d| d.page = 2);
        assert_eq!(r.step("m", 1, &four(), t).unwrap().page, 1);
        r.update("m", |d| d.page = 0);
        assert_eq!(r.step("m", -1, &four(), t).unwrap().page, 1);
    }

    #[test]
    fn the_cycle_advances_only_inside_the_page_list() {
        let t0 = Utc::now();
        let r = listed(10, &["b", "d"], t0);
        assert_eq!(r.current_page("m", &four(), t0 + Duration::seconds(5)), Some(1));
        assert_eq!(r.current_page("m", &four(), t0 + Duration::seconds(11)), Some(3));
        assert_eq!(r.current_page("m", &four(), t0 + Duration::seconds(21)), Some(1));
        assert_eq!(r.current_page("m", &four(), t0 + Duration::seconds(31)), Some(3));
    }

    #[test]
    fn the_cycle_starts_from_the_first_list_entry_when_on_a_page_outside_it() {
        let t0 = Utc::now();
        let r = listed(10, &["b", "d"], t0);
        r.update("m", |d| d.page = 2);
        assert_eq!(r.current_page("m", &four(), t0 + Duration::seconds(1)), Some(1));
        assert_eq!(r.get("m").unwrap().page, 1);
    }

    #[test]
    fn a_hold_shows_its_page_even_outside_the_list() {
        let t0 = Utc::now();
        let r = listed(0, &["b", "d"], t0);
        r.hold_all("a", std::time::Duration::from_secs(20), "ring", t0);
        assert_eq!(r.current_page("m", &four(), t0 + Duration::seconds(5)), Some(0));
        assert_eq!(r.current_page("m", &four(), t0 + Duration::seconds(25)), Some(1));
    }

    #[test]
    fn unknown_page_names_are_rejected_by_name() {
        let r = Registry::new(0);
        let t = Utc::now();
        r.touch("m", 320, 170, t);
        let err = r.set_pages("m", vec!["b".into(), "x".into(), "y".into()], &four(), t).unwrap_err();
        assert_eq!(err, PageError::Unknown(vec!["x".into(), "y".into()]));
        assert!(err.to_string().contains("\"x\"") && err.to_string().contains("\"y\""), "{err}");
        assert!(r.get("m").unwrap().pages.is_empty());
    }

    #[test]
    fn a_page_listed_twice_is_rejected() {
        let r = Registry::new(0);
        let t = Utc::now();
        r.touch("m", 320, 170, t);
        let err = r.set_pages("m", vec!["b".into(), "b".into()], &four(), t).unwrap_err();
        assert_eq!(err, PageError::Duplicate("b".into()));
    }

    #[test]
    fn an_empty_list_means_every_page() {
        let t = Utc::now();
        let r = listed(0, &["b", "d"], t);
        let d = r.set_pages("m", vec![], &four(), t).unwrap().unwrap();
        assert!(d.pages.is_empty());
        assert_eq!(r.step("m", 1, &four(), t).unwrap().page, 2);
        assert_eq!(r.step("m", 1, &four(), t).unwrap().page, 3);
        assert_eq!(r.step("m", 1, &four(), t).unwrap().page, 0);
    }

    #[test]
    fn set_pages_on_an_unknown_device_is_none() {
        assert_eq!(Registry::new(0).set_pages("ghost", vec![], &four(), Utc::now()), Ok(None));
    }

    #[test]
    fn set_page_by_name_stays_inside_the_list() {
        let t = Utc::now();
        let r = listed(0, &["b", "d"], t);
        assert_eq!(r.set_page("m", "d", &four(), t).unwrap().unwrap().page, 3);
        assert_eq!(r.set_page("m", "a", &four(), t), Err(PageError::NotInList("a".into())));
        assert_eq!(r.set_page("m", "zz", &four(), t), Err(PageError::Unknown(vec!["zz".into()])));
        assert_eq!(r.get("m").unwrap().page, 3);
        let free = Registry::new(0);
        free.touch("m", 320, 170, t);
        assert_eq!(free.set_page("m", "c", &four(), t).unwrap().unwrap().page, 2);
    }

    #[test]
    fn devices_saved_before_page_lists_still_load() {
        let json = r#"{"m": {"id": "m", "width": 320, "height": 170, "page": 1, "cycle_s": 10,
            "first_seen": "2026-10-25T00:00:00Z", "last_seen": "2026-10-25T00:00:00Z",
            "page_since": "2026-10-25T00:00:00Z"}}"#;
        let devices: BTreeMap<String, Device> = serde_json::from_str(json).unwrap();
        assert!(devices["m"].pages.is_empty());
    }
}
