use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use peephole_core::scheduler::PollTriggers;
use peephole_core::{Config, Event, Page, Registry, Store};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::settings::Settings;

pub type AppState = Arc<Hub>;

/// What every request handler shares.
#[derive(Debug)]
pub struct Hub {
    pub cfg: Config,
    pub store: Store,
    pub registry: Registry,
    pub demo: bool,
    pub firmware_dir: Option<PathBuf>,
    pub public_url: Option<String>,
    pub polls: PollTriggers,
}

impl Hub {
    pub fn new(settings: &Settings, cfg: Config, store: Store, registry: Registry, polls: PollTriggers) -> AppState {
        Arc::new(Self {
            cfg,
            store,
            registry,
            polls,
            demo: settings.demo,
            firmware_dir: settings.firmware_dir.clone(),
            public_url: settings.public_url.clone(),
        })
    }

    pub fn page_names(&self) -> Vec<String> {
        self.cfg.pages.iter().map(|p| p.name.clone()).collect()
    }

    pub fn page_index(&self, name: &str) -> Option<usize> {
        self.cfg.pages.iter().position(|p| p.name == name)
    }

    pub fn page(&self, index: usize) -> Option<&Page> {
        self.cfg.pages.get(index)
    }

    /// Per plugin: its name and whether its last poll did not fail.
    pub fn sources(&self) -> Vec<(String, bool)> {
        self.cfg
            .plugins
            .iter()
            .map(|p| {
                let name = p.name().to_owned();
                let ok = self.store.num(&format!("{name}/_ok")) != Some(0.0);
                (name, ok)
            })
            .collect()
    }
}

pub fn open_registry(settings: &Settings, cfg: &Config) -> Result<Registry> {
    let registry = Registry::new(cfg.hub.cycle_s);
    match &settings.state_file {
        Some(path) => registry.with_file(path).with_context(|| format!("loading PEEPHOLE_STATE {}", path.display())),
        None => Ok(registry),
    }
}

/// Apply every plugin event to all devices until the channel closes.
pub fn spawn_event_holds(registry: Registry, mut events: broadcast::Receiver<Event>) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => registry.hold_all(&ev.page, ev.hold, &ev.reason, Utc::now()),
                Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(skipped = n, "event backlog dropped"),
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use peephole_core::Value;

    use super::*;

    fn settings() -> Settings {
        Settings {
            config: None,
            listen: "127.0.0.1:0".parse().unwrap(),
            demo: false,
            state_file: None,
            firmware_dir: None,
            public_url: None,
        }
    }

    fn config() -> Config {
        Config::parse(
            "hub: {cycle_s: 0}\nplugins: [{kind: clock}, {kind: demo, name: d}]\npages: [{name: a}, {name: b}]\n",
            |_| None,
        )
        .unwrap()
    }

    #[test]
    fn page_lookup_by_name_and_index() {
        let hub = Hub::new(&settings(), config(), Store::new(), Registry::new(0), PollTriggers::default());
        assert_eq!(hub.page_names(), vec!["a", "b"]);
        assert_eq!(hub.page_index("b"), Some(1));
        assert_eq!(hub.page_index("zz"), None);
        assert_eq!(hub.page(1).unwrap().name, "b");
        assert!(hub.page(2).is_none());
    }

    #[test]
    fn sources_follow_each_plugins_last_poll_by_name() {
        let store = Store::new();
        store.set("d/_ok", Value::Number(0.0));
        store.set("clock/_ok", Value::Number(1.0));
        let hub = Hub::new(&settings(), config(), store, Registry::new(0), PollTriggers::default());
        assert_eq!(hub.sources(), vec![("clock".to_owned(), true), ("d".to_owned(), false)]);
    }

    #[test]
    fn registry_uses_the_config_cycle_and_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices.json");
        let mut s = settings();
        s.state_file = Some(path.clone());
        let mut cfg = config();
        cfg.hub.cycle_s = 7;
        let reg = open_registry(&s, &cfg).unwrap();
        assert_eq!(reg.touch("m", 320, 170, Utc::now()).cycle_s, 7);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn events_hold_every_device_on_the_event_page() {
        let reg = Registry::new(0);
        reg.touch("m", 320, 170, Utc::now());
        let (tx, rx) = broadcast::channel(4);
        let task = spawn_event_holds(reg.clone(), rx);
        tx.send(Event { page: "b".into(), hold: Duration::from_secs(30), reason: "ring".into() }).unwrap();
        for _ in 0..50 {
            if reg.get("m").unwrap().hold.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let hold = reg.get("m").unwrap().hold.expect("hold applied");
        assert_eq!(hold.page, "b");
        assert_eq!(hold.reason, "ring");
        drop(tx);
        task.await.unwrap();
    }
}
