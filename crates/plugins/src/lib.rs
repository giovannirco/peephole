//! Peephole plugins. [`build`] turns a config entry into a running plugin.

use std::sync::Arc;

use anyhow::Context;

use peephole_core::Plugin;
use peephole_core::config::PluginEntry;

pub mod cliproxyapi;
pub mod clock;
pub mod demo;
pub mod homarr;
pub mod home_assistant;
pub mod homepage;
pub mod forgejo;
pub mod github;
mod http;
pub mod http_json;
mod insecure_tls;
mod key;
pub mod kubernetes;
pub mod mempool;
mod partial;
mod period;
pub mod prometheus;
pub mod proxmox;
pub mod technitium;
pub mod unifi_network;
pub mod unifi_protect;
pub mod uptime_kuma;
pub mod waha;

/// Names of the plugin kinds this build knows.
pub fn kinds() -> &'static [&'static str] {
    &["clock", "demo", "prometheus", "http_json", "mempool", "unifi_protect", "proxmox", "technitium", "homepage", "homarr", "uptime_kuma", "unifi_network", "cliproxyapi", "home_assistant", "waha", ]
pub mod x;
    &["clock", "demo", "prometheus", "http_json", "mempool", "unifi_protect", "forgejo", "github", "x", "kubernetes"]
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
        "proxmox" => make(entry, proxmox::Proxmox::new),
        "technitium" => make(entry, technitium::Technitium::new),
        "homepage" => make(entry, homepage::Homepage::new),
        "homarr" => make(entry, homarr::Homarr::new),
        "uptime_kuma" => make(entry, uptime_kuma::UptimeKuma::new),
        "unifi_network" => make(entry, unifi_network::UnifiNetwork::new),
        "cliproxyapi" => make(entry, cliproxyapi::Cliproxyapi::new),
        "home_assistant" => make(entry, home_assistant::HomeAssistant::new),
        "waha" => make(entry, waha::Waha::new),
        "forgejo" => make(entry, forgejo::Forgejo::new),
        "github" => make(entry, github::Github::new),
        "x" => make(entry, x::X::new),
        "kubernetes" => make(entry, kubernetes::Kubernetes::new),
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
            "{kind: proxmox, hosts: [{name: a, url: 'https://pve-a.example.invalid:8006', token_id: 'u@pve!t', token_secret: s}]}",
            "{kind: technitium, url: 'http://dns.example.invalid:5380', token: t}",
            "{kind: homepage, url: 'https://home.example.invalid'}",
            "{kind: homarr, url: 'https://homarr.example.invalid', api_key: k}",
            "{kind: uptime_kuma, url: 'https://kuma.example.invalid', api_key: k}",
            "{kind: unifi_network, url: 'https://unifi.example.invalid', username: u, password: p}",
            "{kind: cliproxyapi, url: 'http://cpa.example.invalid:8317', management_key: k}",
            "{kind: home_assistant, url: 'http://ha.example.invalid:8123', token: t}",
            "{kind: waha, servers: [{name: a, url: 'http://w.example.invalid', api_key: k}]}",
            "{kind: forgejo, url: 'https://forgejo.example.invalid', token: t}",
            "{kind: github, user: octo}",
            "{kind: x, bearer_token: t, username: octo}",
            "{kind: kubernetes, clusters: [{name: a, url: 'https://k8s.example.invalid:6443', token: t, insecure: true}]}",
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
