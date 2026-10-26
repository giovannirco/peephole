//! gethomepage/homepage (verified against v2.7.0 source).
//!
//! - `GET /api/services` (`src/pages/api/services/index.js` → `servicesResponse()` in
//!   `src/utils/config/api-response.js`): `[{name, type, services: [{name, href, ping?, siteMonitor?, …}],
//!   groups: [<same shape>]}]`; nested groups come from `parseServicesToGroups` in
//!   `src/utils/config/service-helpers.js`. Only `name`, `ping` and `siteMonitor` are read.
//! - `GET /api/ping?groupName=&serviceName=` (`src/pages/api/ping.js`): the `ping` npm package's
//!   `PingResponse` (`alive`, `time` in ms or `"unknown"`); HTTP 400 `{error}` when the probe fails.
//! - `GET /api/siteMonitor?groupName=&serviceName=` (`src/pages/api/siteMonitor.js`): `{status, latency}`;
//!   HTTP 400 `{error}` when the request fails. Like `src/components/services/site-monitor.jsx`,
//!   a status above 403 is down.
//!
//! `groupName` is the service's own group: `findGroupByName` searches nested groups by name.
//! A service with both checks is probed with `siteMonitor` only. Every route sits behind
//! `withAuth` (`src/utils/auth.js`): the Host header must be in `HOMEPAGE_ALLOWED_HOSTS`, which
//! the configured URL's host must satisfy.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures::StreamExt;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Homepage base URL, e.g. `https://home.example.invalid`.
    pub url: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub insecure: bool,
    /// Call `ping`/`siteMonitor` for the services that declare them.
    #[serde(default = "default_true")]
    pub check: bool,
    /// Checks in flight at once.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Only these groups (and the groups nested in them).
    #[serde(default)]
    pub groups: Option<Vec<String>>,
    /// Names kept in `offline_list`.
    #[serde(default = "default_list_max")]
    pub list_max: usize,
}

fn default_interval() -> u64 {
    60
}
fn default_true() -> bool {
    true
}
fn default_concurrency() -> usize {
    6
}
fn default_list_max() -> usize {
    5
}

#[derive(Deserialize)]
struct Group {
    name: String,
    #[serde(default)]
    services: Vec<Service>,
    #[serde(default)]
    groups: Vec<Group>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    name: String,
    #[serde(default)]
    ping: Option<String>,
    #[serde(default)]
    site_monitor: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Check {
    Ping,
    SiteMonitor,
}

impl Check {
    fn of(service: &Service) -> Option<Self> {
        let declared = |target: &Option<String>| target.as_deref().is_some_and(|t| !t.is_empty());
        if declared(&service.site_monitor) {
            Some(Self::SiteMonitor)
        } else if declared(&service.ping) {
            Some(Self::Ping)
        } else {
            None
        }
    }

    fn route(self) -> &'static str {
        match self {
            Self::Ping => "api/ping",
            Self::SiteMonitor => "api/siteMonitor",
        }
    }
}

struct Entry {
    group: String,
    name: String,
    key: String,
    check: Option<Check>,
}

struct Probe {
    online: bool,
    latency_ms: Option<f64>,
    status: Option<f64>,
}

#[derive(Deserialize)]
struct PingResponse {
    alive: bool,
    time: serde_json::Value,
}

#[derive(Deserialize)]
struct SiteMonitorResponse {
    status: f64,
    latency: f64,
}

const SITE_MONITOR_DOWN_ABOVE: f64 = 403.0;

pub struct Homepage {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    base: reqwest::Url,
    check: bool,
    concurrency: usize,
    groups: Option<Vec<String>>,
    list_max: usize,
}

impl Homepage {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(s) => crate::period::seconds(s, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        anyhow::ensure!(settings.concurrency > 0, "concurrency must be greater than 0");
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &Default::default(), None, settings.insecure)?,
            base: crate::http::base_url(&settings.url)?,
            check: settings.check,
            concurrency: settings.concurrency,
            groups: settings.groups,
            list_max: settings.list_max,
        })
    }

    async fn probe(&self, entry: &Entry, check: Check) -> anyhow::Result<Probe> {
        let mut url = crate::http::join(&self.base, check.route());
        url.query_pairs_mut().append_pair("groupName", &entry.group).append_pair("serviceName", &entry.name);
        let path = url.path().to_owned();
        let response =
            self.client.get(url).send().await.map_err(crate::http::scrub).with_context(|| format!("GET {path}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::BAD_REQUEST {
            return Ok(Probe { online: false, latency_ms: None, status: None });
        }
        anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}");
        match check {
            Check::Ping => {
                let ping: PingResponse = response.json().await?;
                Ok(Probe { online: ping.alive, latency_ms: ping.time.as_f64().filter(|_| ping.alive), status: None })
            }
            Check::SiteMonitor => {
                let monitor: SiteMonitorResponse = response.json().await?;
                Ok(Probe {
                    online: monitor.status <= SITE_MONITOR_DOWN_ABOVE,
                    latency_ms: Some(monitor.latency),
                    status: Some(monitor.status),
                })
            }
        }
    }
}

fn collect<'a>(groups: &'a [Group], filter: Option<&[String]>, out: &mut Vec<&'a Group>) {
    for group in groups {
        let selected = filter.is_none_or(|names| names.contains(&group.name));
        if selected {
            out.push(group);
        }
        collect(&group.groups, if selected { None } else { filter }, out);
    }
}

fn entries(groups: &[&Group]) -> Vec<Entry> {
    let mut seen = std::collections::HashMap::<&str, usize>::new();
    for service in groups.iter().flat_map(|g| &g.services) {
        *seen.entry(&service.name).or_default() += 1;
    }
    groups
        .iter()
        .flat_map(|group| group.services.iter().map(move |service| (group, service)))
        .map(|(group, service)| {
            let name = crate::key::sanitize(&service.name);
            let key = if seen[service.name.as_str()] > 1 {
                format!("{}-{name}", crate::key::sanitize(&group.name))
            } else {
                name
            };
            Entry { group: group.name.clone(), name: service.name.clone(), key, check: Check::of(service) }
        })
        .collect()
}

fn number(n: usize) -> Value {
    Value::Number(n as f64)
}

#[async_trait]
impl Plugin for Homepage {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let tree: Vec<Group> =
            crate::http::get(&self.client, crate::http::join(&self.base, "api/services")).await?.json().await?;
        let mut groups = Vec::new();
        collect(&tree, self.groups.as_deref(), &mut groups);
        let entries = entries(&groups);
        let checks: Vec<(usize, Check)> = if self.check {
            entries.iter().enumerate().filter_map(|(i, e)| e.check.map(|c| (i, c))).collect()
        } else {
            Vec::new()
        };
        let listed = &entries;
        let mut results: Vec<(usize, anyhow::Result<Probe>)> = futures::stream::iter(checks.iter().copied())
            .map(|(i, check)| async move { (i, self.probe(&listed[i], check).await) })
            .buffer_unordered(self.concurrency)
            .collect()
            .await;
        results.sort_by_key(|(i, _)| *i);

        let mut partial = crate::partial::Partial::new(&self.name, "checks");
        let mut probes: Vec<Option<Probe>> = entries.iter().map(|_| None).collect();
        for (i, result) in results {
            let entry = &entries[i];
            match result {
                Ok(probe) => {
                    let mut values = vec![(format!("service/{}/online", entry.key), number(probe.online as usize))];
                    if let Some(ms) = probe.latency_ms {
                        values.push((format!("service/{}/latency_ms", entry.key), Value::Number(ms)));
                    }
                    if let Some(status) = probe.status {
                        values.push((format!("service/{}/status", entry.key), Value::Number(status)));
                    }
                    partial.ok(values);
                    probes[i] = Some(probe);
                }
                Err(e) => partial.fail(&format!("{} {}", entry.check.map_or("", Check::route), entry.key), &e),
            }
        }
        let mut values = partial.finish()?;
        values.extend(summary(&groups, &entries, &probes, checks.len(), self.list_max));
        Ok(values)
    }
}

fn state(probe: &Option<Probe>) -> Option<bool> {
    probe.as_ref().map(|p| p.online)
}

fn count(probes: &[&Option<Probe>], online: bool) -> Value {
    number(probes.iter().filter(|p| state(p) == Some(online)).count())
}

fn summary(
    groups: &[&Group],
    entries: &[Entry],
    probes: &[Option<Probe>],
    checked: usize,
    list_max: usize,
) -> Vec<(String, Value)> {
    let all: Vec<&Option<Probe>> = probes.iter().collect();
    let online = all.iter().filter(|p| state(p) == Some(true)).count();
    let ratio = if checked == 0 { 0.0 } else { online as f64 / checked as f64 };
    let mut out = vec![
        ("groups".to_owned(), number(groups.len())),
        ("services_total".to_owned(), number(entries.len())),
        ("checked".to_owned(), number(checked)),
        ("online".to_owned(), number(online)),
        ("offline".to_owned(), count(&all, false)),
        ("online_ratio".to_owned(), Value::Number(ratio)),
    ];
    for group in groups {
        let key = crate::key::sanitize(&group.name);
        let mine: Vec<&Option<Probe>> =
            entries.iter().zip(probes).filter(|(e, _)| e.group == group.name).map(|(_, p)| p).collect();
        out.push((format!("group/{key}/services"), number(group.services.len())));
        out.push((format!("group/{key}/online"), count(&mine, true)));
        out.push((format!("group/{key}/offline"), count(&mine, false)));
    }
    let down: Vec<&str> = entries
        .iter()
        .zip(probes)
        .filter(|(_, p)| state(p) == Some(false))
        .map(|(e, _)| e.name.as_str())
        .take(list_max)
        .collect();
    out.push(("offline_list".to_owned(), Value::Text(down.join(", "))));
    let slowest = entries
        .iter()
        .zip(probes)
        .filter_map(|(e, p)| p.as_ref().filter(|p| p.online).and_then(|p| p.latency_ms).map(|ms| (e, ms)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    if let Some((entry, ms)) = slowest {
        out.push(("slowest".to_owned(), Value::Text(entry.name.clone())));
        out.push(("slowest_ms".to_owned(), Value::Number(ms)));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Instant;

    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn service(name: &str, extra: serde_json::Value) -> serde_json::Value {
        let mut s = json!({
            "name": name,
            "href": format!("https://{}.example.invalid", name.to_lowercase()),
            "icon": "mdi-server",
            "description": format!("{name} service"),
            "weight": 100,
            "type": "service",
            "widgets": []
        });
        s.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        s
    }

    fn group(name: &str, services: Vec<serde_json::Value>, groups: Vec<serde_json::Value>) -> serde_json::Value {
        json!({"name": name, "services": services, "type": "group", "groups": groups})
    }

    fn fixture() -> serde_json::Value {
        let storage = group("Storage", vec![service("MinIO", json!({"ping": "minio.example.invalid"}))], vec![]);
        json!([
            group(
                "Infra",
                vec![
                    service("Proxmox", json!({"ping": "pve.example.invalid"})),
                    service("NAS", json!({"siteMonitor": "https://nas.example.invalid"})),
                ],
                vec![storage],
            ),
            group(
                "Media",
                vec![
                    service(
                        "Jellyfin",
                        json!({"ping": "http://jf.example.invalid:8096", "widgets": [{"type": "jellyfin"}]})
                    ),
                    service("Docs", json!({})),
                ],
                vec![],
            ),
        ])
    }

    async fn services(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/api/services"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn probe(server: &MockServer, route: &str, group: &str, name: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/api/{route}")))
            .and(query_param("groupName", group))
            .and(query_param("serviceName", name))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn alive(time: f64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "inputHost": "pve.example.invalid", "host": "pve.example.invalid", "alive": true,
            "output": "PING pve.example.invalid (10.0.0.1): 56 data bytes", "time": time,
            "times": [time], "min": "1.000", "max": "1.000", "avg": "1.000", "stddev": "0.000",
            "packetLoss": "0.000", "numeric_host": "10.0.0.1"
        }))
    }

    fn dead() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "inputHost": "jf.example.invalid", "host": "unknown", "alive": false, "output": "unknown",
            "time": "unknown", "times": [], "min": "unknown", "max": "unknown", "avg": "unknown",
            "stddev": "unknown", "packetLoss": "100.000", "numeric_host": "unknown"
        }))
    }

    fn errored() -> ResponseTemplate {
        ResponseTemplate::new(400).set_body_json(json!({"error": "Error attempting ping, see logs."}))
    }

    fn monitor(status: u16, latency: f64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"status": status, "latency": latency}))
    }

    async fn dashboard() -> MockServer {
        let server = MockServer::start().await;
        services(&server, fixture()).await;
        probe(&server, "ping", "Infra", "Proxmox", alive(12.0)).await;
        probe(&server, "siteMonitor", "Infra", "NAS", monitor(200, 87.5)).await;
        probe(&server, "ping", "Storage", "MinIO", errored()).await;
        probe(&server, "ping", "Media", "Jellyfin", dead()).await;
        server
    }

    fn plugin(server: &MockServer, extra: &str) -> Homepage {
        let yaml = format!("url: {}\n{extra}", server.uri());
        Homepage::new("home", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Homepage) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn counts_groups_services_and_check_results() {
        let server = dashboard().await;
        let got = poll(&plugin(&server, "")).await;
        let n = |k: &str| got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} missing in {got:?}"));
        assert_eq!(n("groups"), 3.0);
        assert_eq!(n("services_total"), 5.0);
        assert_eq!(n("checked"), 4.0);
        assert_eq!(n("online"), 2.0);
        assert_eq!(n("offline"), 2.0);
        assert_eq!(n("online_ratio"), 0.5);
        assert_eq!(n("group/Infra/services"), 2.0);
        assert_eq!(n("group/Infra/online"), 2.0);
        assert_eq!(n("group/Infra/offline"), 0.0);
        assert_eq!(n("group/Storage/services"), 1.0);
        assert_eq!(n("group/Storage/offline"), 1.0);
        assert_eq!(n("group/Media/services"), 2.0);
        assert_eq!(n("group/Media/online"), 0.0);
        assert_eq!(n("group/Media/offline"), 1.0);
        assert_eq!(n("service/Proxmox/online"), 1.0);
        assert_eq!(n("service/Proxmox/latency_ms"), 12.0);
        assert!(!got.contains_key("service/Proxmox/status"));
        assert_eq!(n("service/NAS/online"), 1.0);
        assert_eq!(n("service/NAS/latency_ms"), 87.5);
        assert_eq!(n("service/NAS/status"), 200.0);
        assert_eq!(n("service/MinIO/online"), 0.0);
        assert!(!got.contains_key("service/MinIO/latency_ms"));
        assert_eq!(n("service/Jellyfin/online"), 0.0);
        assert!(!got.contains_key("service/Jellyfin/latency_ms"));
        assert!(!got.keys().any(|k| k.starts_with("service/Docs/")));
        assert_eq!(got["offline_list"], Value::Text("MinIO, Jellyfin".into()));
        assert_eq!(got["slowest"], Value::Text("NAS".into()));
        assert_eq!(n("slowest_ms"), 87.5);
        assert_eq!(n("_checks_ok"), 4.0);
        assert_eq!(n("_checks_failed"), 0.0);
    }

    #[tokio::test]
    async fn never_emits_hrefs_or_ping_targets() {
        let server = dashboard().await;
        let got = poll(&plugin(&server, "")).await;
        for (key, value) in &got {
            assert!(!key.contains("example.invalid") && !key.contains("http"), "{key}");
            if let Value::Text(text) = value {
                assert!(!text.contains("example.invalid") && !text.contains("10.0.0.1"), "{key} = {text}");
            }
        }
    }

    #[tokio::test]
    async fn site_monitor_is_down_only_above_403_like_the_dashboard() {
        let server = MockServer::start().await;
        let body = json!([group(
            "Web",
            vec![
                service("Blog", json!({"siteMonitor": "https://blog.example.invalid"})),
                service("Wiki", json!({"siteMonitor": "https://wiki.example.invalid"})),
            ],
            vec![],
        )]);
        services(&server, body).await;
        probe(&server, "siteMonitor", "Web", "Blog", monitor(503, 40.0)).await;
        probe(&server, "siteMonitor", "Web", "Wiki", monitor(403, 9.0)).await;
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["service/Blog/online"], Value::Number(0.0));
        assert_eq!(got["service/Blog/status"], Value::Number(503.0));
        assert_eq!(got["service/Wiki/online"], Value::Number(1.0));
        assert_eq!(got["service/Wiki/status"], Value::Number(403.0));
        assert_eq!(got["offline_list"], Value::Text("Blog".into()));
    }

    #[tokio::test]
    async fn a_failing_check_is_a_failed_part_not_an_offline_service() {
        let server = MockServer::start().await;
        let body = json!([group(
            "Infra",
            vec![
                service("A", json!({"ping": "a.example.invalid"})),
                service("B", json!({"ping": "b.example.invalid"}))
            ],
            vec![],
        )]);
        services(&server, body).await;
        probe(&server, "ping", "Infra", "A", alive(2.0)).await;
        probe(&server, "ping", "Infra", "B", ResponseTemplate::new(500)).await;
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["online"], Value::Number(1.0));
        assert_eq!(got["offline"], Value::Number(0.0));
        assert_eq!(got["checked"], Value::Number(2.0));
        assert!(!got.contains_key("service/B/online"));
        assert_eq!(got["_checks_ok"], Value::Number(1.0));
        assert_eq!(got["_checks_failed"], Value::Number(1.0));
    }

    #[tokio::test]
    async fn the_service_list_failing_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/services"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "Host validation failed."})))
            .mount(&server)
            .await;
        let err = format!("{:#}", plugin(&server, "").poll().await.unwrap_err());
        assert!(err.contains("/api/services") && err.contains("400"), "{err}");
    }

    #[tokio::test]
    async fn repeated_names_are_prefixed_with_their_group() {
        let server = MockServer::start().await;
        let body = json!([
            group(
                "Infra",
                vec![service("Grafana", json!({"ping": "g1.example.invalid"})), service("Omni", json!({}))],
                vec![]
            ),
            group("Apps", vec![service("Grafana", json!({"ping": "g2.example.invalid"}))], vec![]),
        ]);
        services(&server, body).await;
        probe(&server, "ping", "Infra", "Grafana", alive(1.0)).await;
        probe(&server, "ping", "Apps", "Grafana", dead()).await;
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["service/Infra-Grafana/online"], Value::Number(1.0));
        assert_eq!(got["service/Apps-Grafana/online"], Value::Number(0.0));
        assert!(!got.contains_key("service/Grafana/online"));
        assert_eq!(got["services_total"], Value::Number(3.0));
    }

    #[tokio::test]
    async fn the_groups_filter_keeps_named_groups_and_their_subgroups() {
        let server = MockServer::start().await;
        services(&server, fixture()).await;
        probe(&server, "ping", "Infra", "Proxmox", alive(12.0)).await;
        probe(&server, "siteMonitor", "Infra", "NAS", monitor(200, 87.5)).await;
        probe(&server, "ping", "Storage", "MinIO", alive(3.0)).await;
        Mock::given(path("/api/ping"))
            .and(query_param("serviceName", "Jellyfin"))
            .respond_with(dead())
            .expect(0)
            .mount(&server)
            .await;
        let got = poll(&plugin(&server, "groups: [Infra]")).await;
        assert_eq!(got["groups"], Value::Number(2.0));
        assert_eq!(got["services_total"], Value::Number(3.0));
        assert_eq!(got["online"], Value::Number(3.0));
        assert!(!got.keys().any(|k| k.starts_with("group/Media/")));
        assert_eq!(got["offline_list"], Value::Text(String::new()));
    }

    #[tokio::test]
    async fn checks_can_be_turned_off() {
        let server = MockServer::start().await;
        services(&server, fixture()).await;
        Mock::given(path("/api/ping")).respond_with(alive(1.0)).expect(0).mount(&server).await;
        Mock::given(path("/api/siteMonitor")).respond_with(monitor(200, 1.0)).expect(0).mount(&server).await;
        let got = poll(&plugin(&server, "check: false")).await;
        assert_eq!(got["services_total"], Value::Number(5.0));
        assert_eq!(got["checked"], Value::Number(0.0));
        assert_eq!(got["online_ratio"], Value::Number(0.0));
        assert!(!got.contains_key("slowest"));
    }

    #[tokio::test]
    async fn the_offline_list_stops_at_list_max() {
        let server = MockServer::start().await;
        let names = ["A", "B", "C"];
        let list = names.iter().map(|n| service(n, json!({"ping": "x.example.invalid"}))).collect();
        services(&server, json!([group("G", list, vec![])])).await;
        for name in names {
            probe(&server, "ping", "G", name, dead()).await;
        }
        let got = poll(&plugin(&server, "list_max: 2")).await;
        assert_eq!(got["offline"], Value::Number(3.0));
        assert_eq!(got["offline_list"], Value::Text("A, B".into()));
    }

    async fn timed_poll(concurrency: usize) -> Duration {
        let server = MockServer::start().await;
        let names: Vec<String> = (0..8).map(|i| format!("S{i}")).collect();
        let list = names.iter().map(|n| service(n, json!({"ping": "x.example.invalid"}))).collect();
        services(&server, json!([group("G", list, vec![])])).await;
        Mock::given(path("/api/ping"))
            .respond_with(alive(1.0).set_delay(Duration::from_millis(200)))
            .mount(&server)
            .await;
        let p = plugin(&server, &format!("concurrency: {concurrency}"));
        let started = Instant::now();
        let got = poll(&p).await;
        assert_eq!(got["online"], Value::Number(8.0));
        started.elapsed()
    }

    #[tokio::test]
    async fn checks_run_at_most_concurrency_at_a_time() {
        let bounded = timed_poll(2).await;
        assert!(bounded >= Duration::from_millis(780), "{bounded:?}");
        let wide = timed_poll(8).await;
        assert!(wide < Duration::from_millis(600), "{wide:?}");
    }

    #[test]
    fn defaults_and_validation() {
        let s: Settings = serde_yaml::from_str("{url: 'http://home.example.invalid'}").unwrap();
        assert!(s.check && !s.insecure && s.groups.is_none() && s.timeout_s.is_none());
        assert_eq!((s.concurrency, s.list_max), (6, 5));
        assert_eq!(Homepage::new("h", s).unwrap().interval(), Duration::from_secs(60));
        let zero: Settings = serde_yaml::from_str("{url: 'http://home.example.invalid', concurrency: 0}").unwrap();
        assert!(Homepage::new("h", zero).is_err());
        let typo = serde_yaml::from_str::<Settings>("{url: 'http://home.example.invalid', grups: [a]}");
        assert!(typo.is_err());
    }
}
