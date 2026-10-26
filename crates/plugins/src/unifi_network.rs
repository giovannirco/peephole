//! UniFi Network application through its classic API, with a local read-only account.
//!
//! Login: `POST /api/auth/login` on a UniFi OS console (cookie `TOKEN`, header
//! `x-csrf-token`), `POST /api/login` on the classic controller (cookies
//! `unifises` and `csrf_token`), JSON `{"username", "password"}`. The session
//! lives in the client's cookie jar; the CSRF token goes back as `X-CSRF-Token`
//! on every request, as unpoller does (it reports read-only accounts refused
//! without it). Login happens on the first poll; a 401 logs in again once and
//! retries.
//!
//! Data, all `GET {prefix}/api/s/{site}/...` with prefix `/proxy/network` on
//! UniFi OS and none on the classic controller, each answering
//! `{"meta": {"rc": "ok", "msg": ...}, "data": [...]}`: `stat/health`,
//! `stat/device` (falling back to `stat/device-basic`) and `stat/sta`.
//! Only aggregates leave the plugin: no WAN IP, MAC, IP, serial, hostname or
//! client name is read. A device temperature is `general_temperature`, else the
//! `temperatures[]` entry named CPU, else the hottest entry.
//!
//! Verified against unpoller/unifi at 7da59ee (`types.go` paths and `path()`,
//! `unifi.go` login and CSRF handling, `site.go` health fields, `uap.go`,
//! `usw.go`, `udm.go`, `uxg.go`, `usg.go` device fields, `clients.go` client
//! fields, `endpoints_data/stat-health.json` and `stat-device-udm.json` samples)
//! and Art-of-WiFi/UniFi-API-client at e1f7f63 (`src/Client.php`: login paths,
//! `TOKEN`/`unifises` cookies, the `/proxy/network` prefix, `stat/health`,
//! `stat/device-basic`, `stat/device`, `meta.rc`/`meta.msg`). unpoller's UDM
//! sample has no `state`, `num_sta` or `system-stats`; those fields are checked
//! against the Go structs only.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

/// No `Debug`: the struct holds the account's password.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Console base URL, e.g. `https://unifi.example.invalid:11443`.
    pub url: String,
    /// A local, read-only account; never logged.
    pub username: String,
    pub password: String,
    #[serde(default = "default_site")]
    pub site: String,
    /// Accept the console's self-signed certificate.
    #[serde(default)]
    pub insecure: bool,
    /// UniFi OS console (UDM, UNVR, UniFi OS Server, Cloud Key Gen2+); `false` for the classic controller.
    #[serde(default = "default_true")]
    pub unifi_os: bool,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// WAN throughput samples kept for `wan/rx_history` and `wan/tx_history`; 0 turns them off.
    #[serde(default = "default_history")]
    pub history: usize,
    /// Read `stat/sta` (one entry per active client) for per-network and per-SSID counts.
    #[serde(default = "default_true")]
    pub clients: bool,
}

fn default_site() -> String {
    "default".into()
}
fn default_true() -> bool {
    true
}
fn default_interval() -> u64 {
    30
}
fn default_history() -> usize {
    60
}

const OS_LOGIN: &str = "api/auth/login";
const OS_PREFIX: &str = "proxy/network";
const CLASSIC_LOGIN: &str = "api/login";
const CSRF: &str = "x-csrf-token";
const UPDATED_CSRF: &str = "x-updated-csrf-token";

/// Login state shared by the requests of a poll. `generation` grows with every
/// login so concurrent requests refused with the same session log in once.
#[derive(Default)]
struct Session {
    generation: u64,
    active: bool,
    csrf: Option<String>,
}

type Values = Vec<(String, Value)>;

pub struct UnifiNetwork {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    login: reqwest::Url,
    api: reqwest::Url,
    username: String,
    password: String,
    clients: bool,
    session: tokio::sync::Mutex<Session>,
    rings: Mutex<Rings>,
}

impl UnifiNetwork {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(s) => crate::period::seconds(s, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        anyhow::ensure!(!settings.site.is_empty(), "site must not be empty");
        let base = crate::http::base_url(&settings.url)?;
        let (login, prefix) = if settings.unifi_os { (OS_LOGIN, OS_PREFIX) } else { (CLASSIC_LOGIN, "") };
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .cookie_store(true)
            .danger_accept_invalid_certs(settings.insecure)
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            client,
            login: crate::http::join(&base, login),
            api: crate::http::join(&base, &format!("{prefix}/api/s/{}", settings.site)),
            username: settings.username,
            password: settings.password,
            clients: settings.clients,
            session: tokio::sync::Mutex::new(Session::default()),
            rings: Mutex::new(Rings::new(settings.history)),
        })
    }

    /// The current session, logging in first when there is none or when
    /// `refused` names the generation a request was just refused with.
    async fn session(&self, refused: Option<u64>) -> anyhow::Result<(u64, Option<String>)> {
        let mut session = self.session.lock().await;
        if session.active && refused != Some(session.generation) {
            return Ok((session.generation, session.csrf.clone()));
        }
        session.active = false;
        session.csrf = self.log_in().await?;
        session.generation += 1;
        session.active = true;
        Ok((session.generation, session.csrf.clone()))
    }

    async fn log_in(&self) -> anyhow::Result<Option<String>> {
        let path = self.login.path().to_owned();
        let body = serde_json::json!({"username": self.username, "password": self.password});
        let response = self
            .client
            .post(self.login.clone())
            .json(&body)
            .send()
            .await
            .map_err(crate::http::scrub)
            .with_context(|| format!("POST {path}"))?;
        let status = response.status();
        anyhow::ensure!(status.is_success(), "POST {path}: HTTP {status}");
        let headers = response.headers();
        let csrf = headers.get(UPDATED_CSRF).or_else(|| headers.get(CSRF));
        Ok(csrf.and_then(|v| v.to_str().ok()).map(str::to_owned))
    }

    async fn send(&self, url: &reqwest::Url, csrf: Option<&str>) -> anyhow::Result<reqwest::Response> {
        let mut request = self.client.get(url.clone());
        if let Some(token) = csrf {
            let mut value = reqwest::header::HeaderValue::from_str(token).context("invalid CSRF token")?;
            value.set_sensitive(true);
            request = request.header(CSRF, value);
        }
        request.send().await.map_err(crate::http::scrub).with_context(|| format!("GET {}", url.path()))
    }

    /// `GET {api}/{endpoint}`, logging in again once on a 401, and unwrap the
    /// `{"meta": {"rc": "ok"}, "data": [...]}` envelope.
    async fn data<T: DeserializeOwned>(&self, endpoint: &str) -> anyhow::Result<Vec<T>> {
        let url = crate::http::join(&self.api, endpoint);
        let path = url.path().to_owned();
        let (generation, csrf) = self.session(None).await?;
        let mut response = self.send(&url, csrf.as_deref()).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let (_, csrf) = self.session(Some(generation)).await?;
            response = self.send(&url, csrf.as_deref()).await?;
        }
        let status = response.status();
        let text = response.text().await.with_context(|| format!("GET {path}: reading the body"))?;
        let mut body: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("GET {path}: HTTP {status}: not a UniFi API response ({e})"))?;
        let rc = body.pointer("/meta/rc").and_then(serde_json::Value::as_str);
        if !status.is_success() || rc != Some("ok") {
            let msg = body.pointer("/meta/msg").and_then(serde_json::Value::as_str).or(rc).unwrap_or("no meta.rc");
            anyhow::bail!("GET {path}: HTTP {status}: {msg}");
        }
        serde_json::from_value(body["data"].take())
            .map_err(|_| anyhow::anyhow!("GET {path}: data does not have the expected shape"))
    }

    async fn health(&self) -> anyhow::Result<Values> {
        let health: Vec<Health> = self.data("stat/health").await?;
        let mut values = health_values(&health);
        let wan = health.iter().find(|h| h.subsystem == "wan");
        if let Some(wan) = wan {
            values.extend(self.rings.lock().expect("wan rings poisoned").push(wan.rx_rate, wan.tx_rate));
        }
        Ok(values)
    }

    /// `stat/device`, falling back to `stat/device-basic`; each attempt is a part of the poll.
    async fn devices(&self) -> Vec<(&'static str, anyhow::Result<Values>)> {
        let full = self.data::<Device>("stat/device").await.map(|d| device_values(&d, true));
        if full.is_ok() {
            return vec![("stat/device", full)];
        }
        let basic = self.data::<Device>("stat/device-basic").await.map(|d| device_values(&d, false));
        vec![("stat/device", full), ("stat/device-basic", basic)]
    }

    async fn stations(&self) -> anyhow::Result<Values> {
        Ok(station_values(&self.data::<Station>("stat/sta").await?))
    }
}

#[async_trait]
impl Plugin for UnifiNetwork {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        self.session(None).await?;
        let stations = async { if self.clients { Some(self.stations().await) } else { None } };
        let (health, devices, stations) = tokio::join!(self.health(), self.devices(), stations);
        let mut parts = vec![("stat/health", health)];
        parts.extend(devices);
        parts.extend(stations.map(|result| ("stat/sta", result)));
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        for (part, result) in parts {
            match result {
                Ok(values) => partial.ok(values),
                Err(e) => partial.fail(part, &e),
            }
        }
        partial.finish()
    }
}

/// Number, numeric string or null, as the controller mixes them (`"5.6"`, `47`).
fn flex<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

/// Bool, `0`/`1` or `"true"`/`"false"`; anything else is false.
fn flex_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Bool(b) => b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
        serde_json::Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        _ => false,
    })
}

/// `cpu`/`mem` are percentages as strings, `uptime` seconds.
#[derive(Deserialize, Default)]
struct SystemStats {
    #[serde(default, deserialize_with = "flex")]
    cpu: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    mem: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    uptime: Option<f64>,
}

/// One subsystem of `stat/health`. `wan_ip`, `gw_mac` and `gw_name` are not read.
#[derive(Deserialize)]
struct Health {
    subsystem: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, deserialize_with = "flex")]
    num_user: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_guest: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_iot: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_ap: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_adopted: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_disconnected: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_sw: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_gw: Option<f64>,
    #[serde(default, rename = "rx_bytes-r", deserialize_with = "flex")]
    rx_rate: Option<f64>,
    #[serde(default, rename = "tx_bytes-r", deserialize_with = "flex")]
    tx_rate: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    latency: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    uptime: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    drops: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    xput_down: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    xput_up: Option<f64>,
    #[serde(default, rename = "gw_system-stats")]
    gw_stats: Option<SystemStats>,
}

impl Health {
    fn ok(&self) -> bool {
        self.status.as_deref() == Some("ok")
    }
}

fn health_values(health: &[Health]) -> Values {
    let find = |name: &str| health.iter().find(|h| h.subsystem == name);
    let (wan, www, lan, wlan) = (find("wan"), find("www"), find("lan"), find("wlan"));
    let mut out = Values::new();
    let mut put = |key: &str, value: Option<f64>| {
        if let Some(v) = value {
            out.push((key.to_owned(), Value::Number(v)));
        }
    };
    let field = |h: Option<&Health>, f: fn(&Health) -> Option<f64>| h.and_then(f);
    let sum = |parts: &[Option<f64>]| parts.iter().copied().flatten().reduce(|a, b| a + b);
    let users = (field(lan, |h| h.num_user), field(wlan, |h| h.num_user));
    put("wan/up", wan.map(|h| if h.ok() { 1.0 } else { 0.0 }));
    put("wan/rx_bytes_s", field(wan, |h| h.rx_rate));
    put("wan/tx_bytes_s", field(wan, |h| h.tx_rate));
    put("wan/latency_ms", field(wan, |h| h.latency));
    put("wan/uptime", field(wan, |h| h.uptime));
    put("wan/drops", field(wan, |h| h.drops).or(field(www, |h| h.drops)));
    put("www/latency_ms", field(www, |h| h.latency));
    put("www/xput_down_mbps", field(www, |h| h.xput_down));
    put("www/xput_up_mbps", field(www, |h| h.xput_up));
    let gateway = wan.and_then(|h| h.gw_stats.as_ref());
    put("gateway/cpu", gateway.and_then(|s| s.cpu).map(|v| v / 100.0));
    put("gateway/mem", gateway.and_then(|s| s.mem).map(|v| v / 100.0));
    put("gateway/uptime", gateway.and_then(|s| s.uptime));
    put("clients", sum(&[users.0, users.1]));
    put("clients_wired", users.0);
    put("clients_wireless", users.1);
    put("clients_guest", sum(&[field(lan, |h| h.num_guest), field(wlan, |h| h.num_guest)]));
    put("clients_iot", sum(&[field(lan, |h| h.num_iot), field(wlan, |h| h.num_iot)]));
    put("aps", field(wlan, |h| h.num_ap));
    put("aps_disconnected", field(wlan, |h| h.num_disconnected));
    put("switches", field(lan, |h| h.num_sw));
    put("gateways", field(wan, |h| h.num_gw));
    put("devices_adopted", sum(&health.iter().map(|h| h.num_adopted).collect::<Vec<_>>()));
    put("devices_disconnected", sum(&health.iter().map(|h| h.num_disconnected).collect::<Vec<_>>()));
    for h in health {
        put(&format!("subsystem/{}/ok", crate::key::sanitize(&h.subsystem)), Some(if h.ok() { 1.0 } else { 0.0 }));
    }
    out
}

#[derive(Deserialize)]
struct Temperature {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "flex")]
    value: Option<f64>,
}

/// One entry of `stat/device` or `stat/device-basic` (which carries only
/// `state`, `type`, `model` and `name` of these). MAC, IP and serial are not read.
#[derive(Deserialize)]
struct Device {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "flex")]
    state: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    uptime: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    num_sta: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    satisfaction: Option<f64>,
    #[serde(default, rename = "system-stats")]
    system_stats: Option<SystemStats>,
    #[serde(default, deserialize_with = "flex")]
    general_temperature: Option<f64>,
    #[serde(default)]
    temperatures: Vec<Temperature>,
    #[serde(default, deserialize_with = "flex_bool")]
    upgradable: bool,
    #[serde(default)]
    version: Option<String>,
}

impl Device {
    fn online(&self) -> bool {
        self.state == Some(1.0)
    }

    /// `general_temperature`, else the `temperatures[]` entry named CPU, else the hottest one.
    fn temperature(&self) -> Option<f64> {
        let named_cpu = || {
            self.temperatures
                .iter()
                .find(|t| t.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case("cpu")))
                .and_then(|t| t.value)
        };
        let hottest = || self.temperatures.iter().filter_map(|t| t.value).reduce(f64::max);
        self.general_temperature.or_else(named_cpu).or_else(hottest)
    }
}

/// Key segments for devices: the name, else the model; repeats get `-2`, `-3`...
fn device_keys(devices: &[Device]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    devices
        .iter()
        .map(|d| {
            let shown = [&d.name, &d.model].into_iter().flatten().find(|s| !s.is_empty()).map_or("device", |s| s);
            let base = crate::key::sanitize(shown);
            let count = seen.entry(base.clone()).or_default();
            *count += 1;
            if *count == 1 { base } else { format!("{base}-{count}") }
        })
        .collect()
}

fn device_values(devices: &[Device], full: bool) -> Values {
    let mut out = Values::new();
    for (device, key) in devices.iter().zip(device_keys(devices)) {
        let key = format!("device/{key}");
        out.push((format!("{key}/online"), Value::Number(if device.online() { 1.0 } else { 0.0 })));
        if let Some(kind) = &device.kind {
            out.push((format!("{key}/type"), Value::Text(kind.clone())));
        }
        if !full {
            continue;
        }
        let stats = device.system_stats.as_ref();
        for (field, value) in [
            ("clients", device.num_sta),
            ("uptime", device.uptime),
            ("cpu", stats.and_then(|s| s.cpu).map(|v| v / 100.0)),
            ("mem", stats.and_then(|s| s.mem).map(|v| v / 100.0)),
            ("satisfaction", device.satisfaction.filter(|v| (0.0..=100.0).contains(v)).map(|v| v / 100.0)),
            ("temp", device.temperature()),
            ("upgradable", Some(if device.upgradable { 1.0 } else { 0.0 })),
        ] {
            if let Some(v) = value {
                out.push((format!("{key}/{field}"), Value::Number(v)));
            }
        }
        if let Some(version) = &device.version {
            out.push((format!("{key}/version"), Value::Text(version.clone())));
        }
    }
    out.push(("devices_total".into(), Value::Number(devices.len() as f64)));
    out.push(("devices_online".into(), Value::Number(devices.iter().filter(|d| d.online()).count() as f64)));
    if full {
        let upgradable = devices.iter().filter(|d| d.upgradable).count();
        out.push(("devices_upgradable".into(), Value::Number(upgradable as f64)));
    }
    out
}

/// One active client of `stat/sta`. Names, hostnames, MACs, IPs and OUIs are not read.
#[derive(Deserialize)]
struct Station {
    #[serde(default, deserialize_with = "flex_bool")]
    is_wired: bool,
    #[serde(default, deserialize_with = "flex_bool")]
    is_guest: bool,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    essid: Option<String>,
    #[serde(default, deserialize_with = "flex")]
    signal: Option<f64>,
    #[serde(default, deserialize_with = "flex")]
    satisfaction: Option<f64>,
}

/// Below this RSSI (dBm) a wireless client counts as weak.
const WEAK_SIGNAL_DBM: f64 = -75.0;

fn station_values(stations: &[Station]) -> Values {
    let mut networks: BTreeMap<String, usize> = BTreeMap::new();
    let mut ssids: BTreeMap<String, usize> = BTreeMap::new();
    for station in stations {
        if let Some(network) = station.network.as_deref().filter(|n| !n.is_empty()) {
            *networks.entry(crate::key::sanitize(network)).or_default() += 1;
        }
        if let Some(essid) = station.essid.as_deref().filter(|e| !station.is_wired && !e.is_empty()) {
            *ssids.entry(crate::key::sanitize(essid)).or_default() += 1;
        }
    }
    let mut out = Values::new();
    for (network, count) in networks {
        out.push((format!("network/{network}/clients"), Value::Number(count as f64)));
    }
    for (ssid, count) in ssids {
        out.push((format!("ssid/{ssid}/clients"), Value::Number(count as f64)));
    }
    let weak = stations.iter().filter(|s| !s.is_wired && s.signal.is_some_and(|v| v < WEAK_SIGNAL_DBM)).count();
    out.push(("clients_weak_signal".into(), Value::Number(weak as f64)));
    out.push(("clients_guest_active".into(), Value::Number(stations.iter().filter(|s| s.is_guest).count() as f64)));
    let scores: Vec<f64> =
        stations.iter().filter_map(|s| s.satisfaction).filter(|v| (0.0..=100.0).contains(v)).collect();
    if !scores.is_empty() {
        let mean = scores.iter().sum::<f64>() / scores.len() as f64;
        out.push(("avg_satisfaction".into(), Value::Number(mean / 100.0)));
    }
    out
}

/// The last `size` WAN throughput samples, one per poll.
struct Rings {
    size: usize,
    rx: VecDeque<f64>,
    tx: VecDeque<f64>,
}

impl Rings {
    fn new(size: usize) -> Self {
        Self { size, rx: VecDeque::with_capacity(size), tx: VecDeque::with_capacity(size) }
    }

    fn push(&mut self, rx: Option<f64>, tx: Option<f64>) -> Values {
        if self.size == 0 {
            return Values::new();
        }
        for (ring, sample) in [(&mut self.rx, rx), (&mut self.tx, tx)] {
            ring.push_back(sample.unwrap_or(f64::NAN));
            while ring.len() > self.size {
                ring.pop_front();
            }
        }
        let series = |ring: &VecDeque<f64>| {
            let padding = std::iter::repeat_n(f64::NAN, self.size - ring.len());
            Value::Series(padding.chain(ring.iter().copied()).collect())
        };
        vec![("wan/rx_history".into(), series(&self.rx)), ("wan/tx_history".into(), series(&self.tx))]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

    const PASSWORD: &str = "pw-not-real-7731";

    fn ok(data: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"meta": {"rc": "ok"}, "data": data}))
    }

    /// `endpoints_data/stat-health.json` from unpoller/unifi, with documentation
    /// addresses and `latency`/`uptime` added to the `wan` entry.
    fn health() -> serde_json::Value {
        json!([
            {"subsystem": "wlan", "num_user": 7, "num_guest": 0, "num_iot": 0, "tx_bytes-r": 0, "rx_bytes-r": 0,
             "status": "ok", "num_ap": 1, "num_adopted": 1, "num_disabled": 0, "num_disconnected": 0, "num_pending": 0},
            {"subsystem": "wan", "num_gw": 1, "num_adopted": 1, "num_disconnected": 0, "num_pending": 0, "status": "ok",
             "wan_ip": "203.0.113.77", "gateways": [], "netmask": "255.255.255.0", "nameservers": [], "num_sta": 8,
             "tx_bytes-r": 34263, "rx_bytes-r": 19465, "latency": 11, "uptime": 492522,
             "gw_mac": "00:00:5e:00:53:a9", "gw_name": "236 Example Road",
             "gw_system-stats": {"cpu": "5.6", "mem": "67.2", "uptime": "492633"},
             "gw_version": "5.0.12.30269", "isp_name": "Example ISP", "isp_organization": "Example ISP, Inc."},
            {"subsystem": "www", "status": "ok", "tx_bytes-r": 34263, "rx_bytes-r": 19465, "latency": 9, "uptime": 492506,
             "drops": 1, "xput_up": 1017.0, "xput_down": 999.0, "speedtest_status": "Success",
             "speedtest_lastrun": 1769686425, "speedtest_ping": 10, "gw_mac": "00:00:5e:00:53:a9"},
            {"subsystem": "lan", "lan_ip": null, "status": "ok", "num_user": 1, "num_guest": 0, "num_iot": 0,
             "tx_bytes-r": 0, "rx_bytes-r": 0, "num_sw": 2, "num_adopted": 2, "num_disconnected": 0, "num_pending": 0},
            {"subsystem": "vpn", "status": "error", "remote_user_enabled": false, "site_to_site_enabled": true,
             "site_to_site_num_active": 0, "site_to_site_num_inactive": 1}
        ])
    }

    fn devices() -> serde_json::Value {
        json!([
            {"_id": "d1", "mac": "00:00:5e:00:53:01", "serial": "SERIAL0001", "ip": "192.0.2.1", "adopted": true,
             "model": "UDMPRO", "name": "UDM Pro", "type": "udm", "version": "5.1.33.34087", "state": 1,
             "uptime": 510587, "num_sta": 8, "upgradable": false,
             "system-stats": {"cpu": "12.5", "mem": "40.0", "uptime": "510590"},
             "temperatures": [{"name": "Local", "type": "board", "value": 50.0}, {"name": "CPU", "type": "cpu", "value": 61.2}]},
            {"_id": "d2", "mac": "00:00:5e:00:53:02", "ip": "192.0.2.2", "adopted": true, "model": "USL24P",
             "name": "Core Switch", "type": "usw", "version": "7.1.26", "state": 1, "uptime": 86400, "num_sta": 5,
             "satisfaction": 100, "general_temperature": 47, "upgradable": true,
             "system-stats": {"cpu": "3", "mem": "30.5", "uptime": "86400"}},
            {"_id": "d3", "mac": "00:00:5e:00:53:03", "ip": "192.0.2.3", "adopted": true, "model": "U6LR",
             "name": "Living Room", "type": "uap", "version": "6.6.77", "state": 1, "uptime": 3600, "num_sta": 7,
             "satisfaction": 92, "upgradable": false, "system-stats": {"cpu": "8.0", "mem": "51.0", "uptime": "3600"}},
            {"_id": "d4", "mac": "00:00:5e:00:53:04", "ip": "192.0.2.4", "adopted": true, "model": "U6LR",
             "type": "uap", "version": "6.6.77", "state": 0, "upgradable": false, "system-stats": {}},
            {"_id": "d5", "mac": "00:00:5e:00:53:05", "ip": "192.0.2.5", "adopted": true, "model": "U6LR", "name": "",
             "type": "uap", "version": "6.6.77", "state": 1, "uptime": 60, "num_sta": 0, "upgradable": false,
             "system-stats": {"cpu": "1", "mem": "20", "uptime": "60"}}
        ])
    }

    fn devices_basic() -> serde_json::Value {
        json!([
            {"mac": "00:00:5e:00:53:01", "state": 1, "adopted": true, "disabled": false, "type": "udm", "model": "UDMPRO", "name": "UDM Pro"},
            {"mac": "00:00:5e:00:53:04", "state": 0, "adopted": true, "disabled": false, "type": "uap", "model": "U6LR"}
        ])
    }

    fn stations() -> serde_json::Value {
        json!([
            {"mac": "00:00:5e:00:53:41", "ip": "192.0.2.41", "hostname": "nas-host", "name": "Family NAS", "oui": "Synology",
             "is_wired": true, "is_guest": false, "network": "Main", "sw_mac": "00:00:5e:00:53:02", "satisfaction": 100},
            {"mac": "00:00:5e:00:53:42", "ip": "192.0.2.42", "hostname": "phone-host", "oui": "Apple", "is_wired": false,
             "is_guest": false, "network": "Main", "essid": "CDD", "ap_mac": "00:00:5e:00:53:03", "signal": -60,
             "satisfaction": 90, "rx_rate": 866000, "tx_rate": 866000},
            {"mac": "00:00:5e:00:53:43", "ip": "192.0.2.43", "hostname": "plug-host", "is_wired": false, "is_guest": false,
             "network": "IoT", "essid": "CDD-Things", "ap_mac": "00:00:5e:00:53:03", "signal": -80, "satisfaction": 40},
            {"mac": "00:00:5e:00:53:44", "ip": "192.0.2.44", "hostname": "visitor-host", "is_wired": false, "is_guest": true,
             "network": "Guest", "essid": "Guest Wi-Fi", "ap_mac": "00:00:5e:00:53:03", "signal": -70, "satisfaction": -1}
        ])
    }

    /// Answers each login with a new `TOKEN` cookie and CSRF token, `tok-1`, `tok-2`...
    struct Login(Arc<AtomicUsize>);

    impl Respond for Login {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            ResponseTemplate::new(200)
                .insert_header("set-cookie", format!("TOKEN=tok-{n}; Path=/; HttpOnly").as_str())
                .insert_header("x-csrf-token", format!("csrf-{n}").as_str())
                .set_body_json(json!({"username": "peephole", "isOwner": false}))
        }
    }

    /// The request's `cookie` header carries `pair`; the jar orders cookies freely.
    struct Cookie(&'static str);

    impl wiremock::Match for Cookie {
        fn matches(&self, request: &Request) -> bool {
            request
                .headers
                .get("cookie")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split("; ").any(|c| c == self.0))
        }
    }

    struct Console {
        server: MockServer,
        logins: Arc<AtomicUsize>,
    }

    /// A UniFi OS console whose data endpoints accept only session `token`.
    async fn console(token: usize, device: ResponseTemplate) -> Console {
        let server = MockServer::start().await;
        let logins = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/auth/login"))
            .and(body_json(json!({"username": "peephole", "password": PASSWORD})))
            .respond_with(Login(logins.clone()))
            .mount(&server)
            .await;
        let site = "/proxy/network/api/s/default";
        for (endpoint, response) in [
            ("stat/health", ok(health())),
            ("stat/device", device),
            ("stat/device-basic", ok(devices_basic())),
            ("stat/sta", ok(stations())),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("{site}/{endpoint}")))
                .and(header("cookie", format!("TOKEN=tok-{token}").as_str()))
                .and(header("x-csrf-token", format!("csrf-{token}").as_str()))
                .respond_with(response)
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(json!({"meta": {"rc": "error", "msg": "api.err.LoginRequired"}, "data": []})),
            )
            .with_priority(10)
            .mount(&server)
            .await;
        Console { server, logins }
    }

    fn plugin(server: &MockServer, extra: &str) -> UnifiNetwork {
        let yaml = format!("url: {}\nusername: peephole\npassword: {PASSWORD}\n{extra}", server.uri());
        UnifiNetwork::new("net", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &UnifiNetwork) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    fn n(got: &BTreeMap<String, Value>, key: &str) -> f64 {
        got.get(key).and_then(Value::as_f64).unwrap_or_else(|| panic!("{key} is not a number in {got:?}"))
    }

    #[tokio::test]
    async fn health_becomes_wan_gateway_and_client_counts() {
        let c = console(1, ok(devices())).await;
        let got = poll(&plugin(&c.server, "")).await;
        for (key, want) in [
            ("wan/up", 1.0),
            ("wan/rx_bytes_s", 19465.0),
            ("wan/tx_bytes_s", 34263.0),
            ("wan/latency_ms", 11.0),
            ("wan/uptime", 492522.0),
            ("wan/drops", 1.0),
            ("www/latency_ms", 9.0),
            ("www/xput_down_mbps", 999.0),
            ("www/xput_up_mbps", 1017.0),
            ("gateway/uptime", 492633.0),
            ("clients", 8.0),
            ("clients_wired", 1.0),
            ("clients_wireless", 7.0),
            ("clients_guest", 0.0),
            ("clients_iot", 0.0),
            ("aps", 1.0),
            ("aps_disconnected", 0.0),
            ("switches", 2.0),
            ("gateways", 1.0),
            ("devices_adopted", 4.0),
            ("devices_disconnected", 0.0),
            ("subsystem/wan/ok", 1.0),
            ("subsystem/www/ok", 1.0),
            ("subsystem/lan/ok", 1.0),
            ("subsystem/wlan/ok", 1.0),
            ("subsystem/vpn/ok", 0.0),
        ] {
            assert_eq!(n(&got, key), want, "{key}");
        }
        assert!((n(&got, "gateway/cpu") - 0.056).abs() < 1e-9);
        assert!((n(&got, "gateway/mem") - 0.672).abs() < 1e-9);
        assert_eq!(n(&got, "_requests_ok"), 3.0);
        assert_eq!(n(&got, "_requests_failed"), 0.0);
    }

    #[tokio::test]
    async fn devices_report_state_load_and_firmware() {
        let c = console(1, ok(devices())).await;
        let got = poll(&plugin(&c.server, "")).await;
        assert_eq!(n(&got, "devices_total"), 5.0);
        assert_eq!(n(&got, "devices_online"), 4.0);
        assert_eq!(n(&got, "devices_upgradable"), 1.0);
        assert_eq!(n(&got, "device/UDM-Pro/online"), 1.0);
        assert_eq!(got["device/UDM-Pro/type"], Value::Text("udm".into()));
        assert_eq!(got["device/UDM-Pro/version"], Value::Text("5.1.33.34087".into()));
        assert_eq!(n(&got, "device/UDM-Pro/clients"), 8.0);
        assert_eq!(n(&got, "device/UDM-Pro/uptime"), 510587.0);
        assert_eq!(n(&got, "device/UDM-Pro/cpu"), 0.125);
        assert_eq!(n(&got, "device/UDM-Pro/mem"), 0.4);
        assert_eq!(n(&got, "device/UDM-Pro/temp"), 61.2);
        assert_eq!(n(&got, "device/UDM-Pro/upgradable"), 0.0);
        assert!(!got.contains_key("device/UDM-Pro/satisfaction"));
        assert_eq!(n(&got, "device/Core-Switch/temp"), 47.0);
        assert_eq!(n(&got, "device/Core-Switch/satisfaction"), 1.0);
        assert_eq!(n(&got, "device/Core-Switch/upgradable"), 1.0);
        assert_eq!(n(&got, "device/Living-Room/satisfaction"), 0.92);
        assert_eq!(n(&got, "device/Living-Room/clients"), 7.0);
        assert!(!got.contains_key("device/Living-Room/temp"));
        assert_eq!(n(&got, "device/U6LR/online"), 0.0);
        assert!(!got.contains_key("device/U6LR/cpu"));
        assert_eq!(n(&got, "device/U6LR-2/online"), 1.0);
        assert_eq!(n(&got, "device/U6LR-2/cpu"), 0.01);
    }

    #[tokio::test]
    async fn clients_are_only_aggregates() {
        let c = console(1, ok(devices())).await;
        let got = poll(&plugin(&c.server, "")).await;
        assert_eq!(n(&got, "network/Main/clients"), 2.0);
        assert_eq!(n(&got, "network/IoT/clients"), 1.0);
        assert_eq!(n(&got, "network/Guest/clients"), 1.0);
        assert_eq!(n(&got, "ssid/CDD/clients"), 1.0);
        assert_eq!(n(&got, "ssid/CDD-Things/clients"), 1.0);
        assert_eq!(n(&got, "ssid/Guest-Wi-Fi/clients"), 1.0);
        assert_eq!(n(&got, "clients_weak_signal"), 1.0);
        assert_eq!(n(&got, "clients_guest_active"), 1.0);
        assert!((n(&got, "avg_satisfaction") - 230.0 / 300.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn nothing_private_reaches_a_key_or_a_value() {
        let c = console(1, ok(devices())).await;
        let got = poll(&plugin(&c.server, "")).await;
        let private = [
            PASSWORD,
            "203.0.113.77",
            "192.0.2.",
            "00:00:5e:00:53",
            "SERIAL0001",
            "236 Example Road",
            "-host",
            "Family NAS",
            "Synology",
            "Apple",
            "Example ISP",
        ];
        for (key, value) in &got {
            let text = format!("{key} {value:?}");
            for secret in private {
                assert!(!text.contains(secret), "{text} leaks {secret}");
            }
        }
    }

    #[tokio::test]
    async fn logs_in_once_and_sends_the_session_on_every_request() {
        let c = console(1, ok(devices())).await;
        let p = plugin(&c.server, "");
        poll(&p).await;
        poll(&p).await;
        assert_eq!(c.logins.load(Ordering::SeqCst), 1);
        let requests = c.server.received_requests().await.unwrap();
        let data: Vec<_> = requests.iter().filter(|r| r.method.as_str() == "GET").collect();
        assert_eq!(data.len(), 6);
        for r in data {
            assert_eq!(r.headers["cookie"], "TOKEN=tok-1", "{}", r.url.path());
            assert_eq!(r.headers["x-csrf-token"], "csrf-1", "{}", r.url.path());
        }
    }

    #[tokio::test]
    async fn an_expired_session_logs_in_again_once_and_retries() {
        let c = console(2, ok(devices())).await;
        let got = poll(&plugin(&c.server, "")).await;
        assert_eq!(c.logins.load(Ordering::SeqCst), 2);
        assert_eq!(n(&got, "wan/up"), 1.0);
        assert_eq!(n(&got, "devices_total"), 5.0);
        assert_eq!(n(&got, "clients_guest_active"), 1.0);
        assert_eq!(n(&got, "_requests_failed"), 0.0);
    }

    #[tokio::test]
    async fn a_session_that_stays_refused_fails_without_looping() {
        let c = console(99, ok(devices())).await;
        let err = format!("{:#}", plugin(&c.server, "").poll().await.unwrap_err());
        assert!(err.contains("401") && err.contains("stat/health") && err.contains("LoginRequired"), "{err}");
        assert_eq!(c.logins.load(Ordering::SeqCst), 3, "first login, one shared retry, one for device-basic");
    }

    #[tokio::test]
    async fn a_refused_login_fails_without_echoing_the_password() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth/login"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "wrong"})))
            .mount(&server)
            .await;
        let err = format!("{:#}", plugin(&server, "").poll().await.unwrap_err());
        assert!(err.contains("/api/auth/login") && err.contains("401") && !err.contains(PASSWORD), "{err}");
        assert!(server.received_requests().await.unwrap().iter().all(|r| r.method.as_str() == "POST"));
    }

    #[tokio::test]
    async fn a_controller_error_quotes_meta_msg() {
        let error = ResponseTemplate::new(400)
            .set_body_json(json!({"meta": {"rc": "error", "msg": "api.err.NoSiteContext"}, "data": []}));
        let c = console(1, error.clone()).await;
        Mock::given(method("GET"))
            .and(path("/proxy/network/api/s/default/stat/device-basic"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"meta": {"rc": "error", "msg": "api.err.Invalid"}})),
            )
            .with_priority(1)
            .mount(&c.server)
            .await;
        let got = poll(&plugin(&c.server, "")).await;
        assert_eq!(n(&got, "_requests_failed"), 2.0);
        assert!(!got.contains_key("devices_total"));
        assert_eq!(n(&got, "wan/up"), 1.0);
        let only = MockServer::start().await;
        Mock::given(method("POST")).respond_with(Login(Arc::new(AtomicUsize::new(0)))).mount(&only).await;
        Mock::given(method("GET")).respond_with(error).mount(&only).await;
        let err = format!("{:#}", plugin(&only, "").poll().await.unwrap_err());
        assert!(err.contains("api.err.NoSiteContext") && err.contains("stat/health"), "{err}");
    }

    #[tokio::test]
    async fn device_basic_stands_in_when_stat_device_fails() {
        let c = console(1, ResponseTemplate::new(500)).await;
        let got = poll(&plugin(&c.server, "")).await;
        assert_eq!(n(&got, "devices_total"), 2.0);
        assert_eq!(n(&got, "devices_online"), 1.0);
        assert_eq!(n(&got, "device/UDM-Pro/online"), 1.0);
        assert_eq!(got["device/U6LR/type"], Value::Text("uap".into()));
        assert!(!got.contains_key("device/UDM-Pro/cpu") && !got.contains_key("devices_upgradable"));
        assert_eq!(n(&got, "_requests_ok"), 3.0);
        assert_eq!(n(&got, "_requests_failed"), 1.0);
    }

    #[tokio::test]
    async fn clients_can_be_turned_off() {
        let c = console(1, ok(devices())).await;
        let got = poll(&plugin(&c.server, "clients: false")).await;
        assert!(!got.keys().any(|k| k.starts_with("network/") || k.starts_with("ssid/") || k == "avg_satisfaction"));
        let requests = c.server.received_requests().await.unwrap();
        assert!(requests.iter().all(|r| !r.url.path().ends_with("stat/sta")));
        assert_eq!(n(&got, "_requests_ok"), 2.0);
    }

    #[tokio::test]
    async fn classic_controller_logs_in_at_api_login_without_the_prefix() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/login"))
            .and(body_json(json!({"username": "peephole", "password": PASSWORD})))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("set-cookie", "unifises=ses-1; Path=/; HttpOnly")
                    .append_header("set-cookie", "csrf_token=csrf-c; Path=/")
                    .set_body_json(json!({"meta": {"rc": "ok"}, "data": []})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/s/branch/stat/health"))
            .and(Cookie("unifises=ses-1"))
            .respond_with(ok(health()))
            .mount(&server)
            .await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).with_priority(10).mount(&server).await;
        let got = poll(&plugin(&server, "unifi_os: false\nsite: branch\nclients: false")).await;
        assert_eq!(n(&got, "wan/up"), 1.0);
        assert_eq!(n(&got, "_requests_failed"), 2.0);
        let requests = server.received_requests().await.unwrap();
        assert!(requests.iter().all(|r| !r.url.path().starts_with("/proxy/")));
    }

    #[tokio::test]
    async fn wan_history_rings_hold_the_last_samples_oldest_first() {
        let c = console(1, ok(devices())).await;
        let p = plugin(&c.server, "history: 3");
        poll(&p).await;
        let first = poll(&p).await;
        let rx = first["wan/rx_history"].as_series().unwrap();
        assert_eq!(rx.len(), 3);
        assert!(rx[0].is_nan());
        assert_eq!(rx[1..], [19465.0, 19465.0]);
        poll(&p).await;
        let full = poll(&p).await;
        assert_eq!(full["wan/rx_history"].as_series().unwrap(), [19465.0; 3]);
        assert_eq!(full["wan/tx_history"].as_series().unwrap(), [34263.0; 3]);
        let fresh = console(1, ok(devices())).await;
        let off = poll(&plugin(&fresh.server, "history: 0")).await;
        assert!(!off.contains_key("wan/rx_history") && !off.contains_key("wan/tx_history"));
    }

    #[test]
    fn defaults() {
        let s: Settings =
            serde_yaml::from_str("{url: 'https://unifi.example.invalid', username: u, password: p}").unwrap();
        assert_eq!(s.site, "default");
        assert!(!s.insecure && s.unifi_os && s.clients && s.timeout_s.is_none());
        assert_eq!(s.history, 60);
        assert_eq!(UnifiNetwork::new("n", s).unwrap().interval(), Duration::from_secs(30));
    }
}
