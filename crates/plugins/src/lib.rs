//! Peephole plugins. [`build`] turns a config entry into a running plugin.

use std::sync::Arc;

use anyhow::Context;

use peephole_core::Plugin;
use peephole_core::config::PluginEntry;

pub mod clock;
pub mod demo;
mod http;
pub mod http_json;
mod insecure_tls;
pub mod job_scout;
mod key;
pub mod mempool;
mod partial;
mod period;
pub mod prometheus;
pub mod qbittorrent;
pub mod servarr;
pub mod unifi_protect;

/// Names of the plugin kinds this build knows.
pub fn kinds() -> &'static [&'static str] {
    &["clock", "demo", "prometheus", "http_json", "mempool", "unifi_protect", "job_scout", "qbittorrent", "servarr"]
}

/// Instantiate the plugin for `entry.kind` with `entry.settings()`.
/// Unknown kinds and invalid settings are errors (the hub refuses to start).
pub fn build(entry: &PluginEntry) -> anyhow::Result<Arc<dyn Plugin>> {
    match entry.kind.as_str() {
        "clock" => make(entry, clock::Clock::new),
        "demo" => make(entry, demo::Demo::new),
        "http_json" => make(entry, http_json::HttpJson::new),
        "prometheus" => make(entry, prometheus::Prometheus::new),
        "mempool" => make(entry, mempool::Mempool::new),
        "unifi_protect" => make(entry, unifi_protect::UnifiProtect::new),
        "job_scout" => make(entry, job_scout::JobScout::new),
        "qbittorrent" => make(entry, qbittorrent::Qbittorrent::new),
        "servarr" => make(entry, servarr::Servarr::new),
        other => anyhow::bail!("plugin {:?}: unknown kind {other:?} (known: {})", entry.name(), kinds().join(", ")),
    }
}

fn make<S, P>(entry: &PluginEntry, new: fn(&str, S) -> anyhow::Result<P>) -> anyhow::Result<Arc<dyn Plugin>>
where
    S: for<'de> serde::Deserialize<'de>,
    P: Plugin + 'static,
{
    let context = || format!("plugin {:?} (kind {}): invalid settings", entry.name(), entry.kind);
    let settings = entry.settings::<S>().with_context(context)?;
    Ok(Arc::new(new(entry.name(), settings).with_context(context)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(yaml: &str) -> PluginEntry {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn builds_known_kinds_under_their_configured_name() {
        assert_eq!(build(&entry("{kind: clock, name: wall, tz: Europe/Lisbon}")).unwrap().name(), "wall");
        assert_eq!(build(&entry("{kind: demo, values: {a: 1}}")).unwrap().name(), "demo");
    }

    #[test]
    fn every_listed_kind_builds() {
        let minimal = [
            "{kind: clock}",
            "{kind: demo, values: {}}",
            "{kind: prometheus, url: 'http://prom.example.invalid', queries: []}",
            "{kind: http_json, url: 'http://api.example.invalid', fields: []}",
            "{kind: mempool, url: 'https://mempool.example.invalid'}",
            "{kind: unifi_protect, url: 'https://nvr.example.invalid', api_key: k, event_page: Door}",
            "{kind: job_scout, url: 'https://jobs.example.invalid', token: t}",
            "{kind: qbittorrent, servers: [{name: nas, url: 'http://qbit.example.invalid', username: u, password: p}]}",
            "{kind: servarr, apps: [{name: movies, kind: radarr, url: 'https://radarr.example.invalid', api_key: k}]}",
        ];
        for (yaml, kind) in minimal.iter().zip(kinds()) {
            assert_eq!(build(&entry(yaml)).unwrap().name(), *kind);
        }
        assert_eq!(minimal.len(), kinds().len());
    }

    #[test]
    fn unknown_kind_is_an_error_listing_the_known_ones() {
        let err = build(&entry("{kind: teapot, name: kettle}")).err().unwrap().to_string();
        assert!(err.contains("teapot") && err.contains("kettle") && err.contains("prometheus"), "{err}");
    }

    #[test]
    fn bad_settings_name_the_plugin() {
        let err = format!("{:#}", build(&entry("{kind: clock, name: wall, tz: Nowhere/Land}")).err().unwrap());
        assert!(err.contains("wall") && err.contains("Nowhere/Land"), "{err}");
        let err = format!("{:#}", build(&entry("{kind: demo, name: fake, values: 3}")).err().unwrap());
        assert!(err.contains("fake"), "{err}");
    }
}
