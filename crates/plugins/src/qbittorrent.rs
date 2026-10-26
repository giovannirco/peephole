//! qBittorrent WebUI API v2: transfer totals, torrent counts by state and
//! category, and a speed history. Never a torrent name, path, hash or tracker.
//!
//! Endpoints: `POST /api/v2/auth/login` (form `username`, `password`, with `Referer`
//! and `Origin` set to the configured URL; success sets the `SID` cookie, a refusal is
//! HTTP 401 on current releases or a 200 `Fails.` without a cookie on 4.x/5.0),
//! `GET /api/v2/app/version` (plain text), `GET /api/v2/transfer/info`,
//! `GET /api/v2/torrents/info`, `GET /api/v2/sync/maindata?rid=0` (`server_state`).
//! A call without a valid session is HTTP 403, answered by one fresh login.
//! Verified against the wiki page "WebUI API (qBittorrent 5.0)" and the sources
//! `src/webui/webapplication.cpp`, `src/webui/api/authcontroller.cpp`,
//! `src/webui/api/synccontroller.cpp` (`global_ratio` is a string, `-` without data)
//! and `src/webui/api/serialize/serialize_torrent.cpp` (state names incl. `stoppedUP`).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, COOKIE, HeaderValue, SET_COOKIE};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// No `Debug`: the servers hold passwords.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub servers: Vec<ServerSettings>,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Speed samples kept for `dl_history` / `up_history`, one per poll.
    #[serde(default = "default_history")]
    pub history: usize,
}

/// No `Debug`: holds a password.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSettings {
    /// Key segment for this server's values.
    pub name: String,
    /// WebUI base URL, e.g. `http://qbit.example.invalid:8080`.
    pub url: String,
    pub username: String,
    /// Sent only in the login form; never logged.
    pub password: String,
    #[serde(default)]
    pub insecure: bool,
}

fn default_interval() -> u64 {
    30
}
fn default_history() -> usize {
    60
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Bucket {
    Downloading,
    Seeding,
    Stalled,
    Paused,
    Queued,
    Checking,
    Errored,
}

/// `allocating`, `moving` and `unknown` belong to no bucket; they still count in `torrents`.
fn bucket(state: &str) -> Option<Bucket> {
    Some(match state {
        "downloading" | "forcedDL" | "metaDL" | "forcedMetaDL" => Bucket::Downloading,
        "uploading" | "forcedUP" | "stalledUP" => Bucket::Seeding,
        "stalledDL" => Bucket::Stalled,
        "pausedUP" | "pausedDL" | "stoppedUP" | "stoppedDL" => Bucket::Paused,
        "queuedUP" | "queuedDL" => Bucket::Queued,
        "checkingUP" | "checkingDL" | "checkingResumeData" => Bucket::Checking,
        "error" | "missingFiles" => Bucket::Errored,
        _ => return None,
    })
}

const BUCKETS: [(Bucket, &str); 7] = [
    (Bucket::Downloading, "downloading"),
    (Bucket::Seeding, "seeding"),
    (Bucket::Stalled, "stalled"),
    (Bucket::Paused, "paused"),
    (Bucket::Queued, "queued"),
    (Bucket::Checking, "checking"),
    (Bucket::Errored, "errored"),
];

#[derive(Deserialize)]
struct TransferInfo {
    dl_info_speed: f64,
    dl_info_data: f64,
    up_info_speed: f64,
    up_info_data: f64,
    dht_nodes: f64,
    connection_status: String,
}

/// One entry of `torrents/info`, reduced to the fields counted here.
#[derive(Deserialize)]
struct Torrent {
    state: String,
    progress: f64,
    amount_left: f64,
    dlspeed: f64,
    upspeed: f64,
    #[serde(default)]
    category: String,
}

#[derive(Deserialize)]
struct MainData {
    server_state: ServerState,
}

#[derive(Deserialize)]
struct ServerState {
    alltime_dl: f64,
    alltime_ul: f64,
    global_ratio: String,
    free_space_on_disk: f64,
}

type Values = Vec<(String, Value)>;

fn number(key: String, n: f64) -> (String, Value) {
    (key, Value::Number(n))
}

#[derive(Default)]
struct Ring {
    dl: VecDeque<f64>,
    up: VecDeque<f64>,
}

impl Ring {
    fn push(&mut self, dl: f64, up: f64, keep: usize) {
        self.dl.push_back(dl);
        self.up.push_back(up);
        while self.dl.len() > keep {
            self.dl.pop_front();
            self.up.pop_front();
        }
    }
}

struct Server {
    key: String,
    base: reqwest::Url,
    client: reqwest::Client,
    username: String,
    password: String,
    sid: tokio::sync::Mutex<Option<String>>,
    ring: Mutex<Ring>,
    history: usize,
}

impl Server {
    fn new(settings: ServerSettings, timeout: Duration, history: usize) -> anyhow::Result<Self> {
        let base = crate::http::base_url(&settings.url)?;
        let headers = BTreeMap::from([
            ("Referer".to_owned(), base.as_str().to_owned()),
            ("Origin".to_owned(), base.origin().ascii_serialization()),
        ]);
        Ok(Self {
            key: crate::key::sanitize(&settings.name),
            client: crate::http::client(timeout, &headers, None, settings.insecure)?,
            base,
            username: settings.username,
            password: settings.password,
            sid: tokio::sync::Mutex::new(None),
            ring: Mutex::new(Ring::default()),
            history,
        })
    }

    fn url(&self, path: &str) -> reqwest::Url {
        crate::http::join(&self.base, path)
    }

    async fn login(&self) -> anyhow::Result<String> {
        let mut form = reqwest::Url::parse("http://form.invalid/").expect("static URL");
        form.query_pairs_mut().append_pair("username", &self.username).append_pair("password", &self.password);
        let body = form.query().unwrap_or_default().to_owned();
        let response = self
            .client
            .post(self.url("api/v2/auth/login"))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("POST /api/v2/auth/login")?;
        let status = response.status();
        anyhow::ensure!(status.is_success(), "POST /api/v2/auth/login: HTTP {status}");
        response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|cookie| cookie.split(';').next()?.trim().strip_prefix("SID=").map(str::to_owned))
            .ok_or_else(|| {
                anyhow::anyhow!("POST /api/v2/auth/login: refused (no SID cookie; check username and password)")
            })
    }

    async fn session(&self, stale: Option<&str>) -> anyhow::Result<String> {
        let mut sid = self.sid.lock().await;
        match sid.as_deref() {
            Some(current) if Some(current) != stale => Ok(current.to_owned()),
            _ => {
                *sid = None;
                let fresh = self.login().await?;
                *sid = Some(fresh.clone());
                Ok(fresh)
            }
        }
    }

    async fn send(&self, url: &reqwest::Url, sid: &str) -> anyhow::Result<reqwest::Response> {
        let mut cookie = HeaderValue::from_str(&format!("SID={sid}")).context("invalid SID cookie")?;
        cookie.set_sensitive(true);
        let response = self.client.get(url.clone()).header(COOKIE, cookie).send().await;
        response.with_context(|| format!("GET {}", url.path()))
    }

    async fn get(&self, url: reqwest::Url) -> anyhow::Result<reqwest::Response> {
        let sid = self.session(None).await?;
        let mut response = self.send(&url, &sid).await?;
        if response.status() == StatusCode::FORBIDDEN {
            let fresh = self.session(Some(&sid)).await?;
            response = self.send(&url, &fresh).await?;
        }
        let status = response.status();
        anyhow::ensure!(status.is_success(), "GET {}: HTTP {status}", url.path());
        Ok(response)
    }

    async fn json<T: DeserializeOwned>(&self, url: reqwest::Url) -> anyhow::Result<T> {
        Ok(self.get(url).await?.json().await?)
    }

    fn k(&self, metric: &str) -> String {
        format!("{}/{metric}", self.key)
    }

    async fn version(&self) -> anyhow::Result<Values> {
        let text = self.get(self.url("api/v2/app/version")).await?.text().await?;
        Ok(vec![(self.k("version"), Value::Text(text.trim().to_owned()))])
    }

    async fn transfer(&self) -> anyhow::Result<Values> {
        let t: TransferInfo = self.json(self.url("api/v2/transfer/info")).await?;
        let (dl, up) = {
            let mut ring = self.ring.lock().expect("speed ring poisoned");
            ring.push(t.dl_info_speed, t.up_info_speed, self.history);
            (ring.dl.iter().copied().collect(), ring.up.iter().copied().collect())
        };
        let connected = if t.connection_status == "connected" { 1.0 } else { 0.0 };
        Ok(vec![
            number(self.k("dl_speed"), t.dl_info_speed),
            number(self.k("up_speed"), t.up_info_speed),
            number(self.k("session_dl_bytes"), t.dl_info_data),
            number(self.k("session_up_bytes"), t.up_info_data),
            number(self.k("dht_nodes"), t.dht_nodes),
            number(self.k("connected"), connected),
            (self.k("connection"), Value::Text(t.connection_status)),
            (self.k("dl_history"), Value::Series(dl)),
            (self.k("up_history"), Value::Series(up)),
        ])
    }

    async fn torrents(&self) -> anyhow::Result<Values> {
        let torrents: Vec<Torrent> = self.json(self.url("api/v2/torrents/info")).await?;
        Ok(self.torrent_values(&torrents))
    }

    fn torrent_values(&self, torrents: &[Torrent]) -> Values {
        let mut values = vec![number(self.k("torrents"), torrents.len() as f64)];
        for (b, metric) in BUCKETS {
            let count = torrents.iter().filter(|t| bucket(&t.state) == Some(b)).count();
            values.push(number(self.k(metric), count as f64));
        }
        let left = torrents.iter().filter(|t| t.progress < 1.0).map(|t| t.amount_left).sum();
        values.push(number(self.k("left_bytes"), left));
        let active = torrents.iter().filter(|t| t.dlspeed + t.upspeed > 0.0).count();
        values.push(number(self.k("active"), active as f64));
        let mut categories: BTreeMap<String, f64> = BTreeMap::new();
        for t in torrents.iter().filter(|t| !t.category.is_empty()) {
            *categories.entry(crate::key::sanitize(&t.category)).or_default() += 1.0;
        }
        for (category, n) in categories {
            values.push(number(self.k(&format!("category/{category}/torrents")), n));
        }
        values
    }

    async fn maindata(&self) -> anyhow::Result<Values> {
        let mut url = self.url("api/v2/sync/maindata");
        url.query_pairs_mut().append_pair("rid", "0");
        let m: MainData = self.json(url).await?;
        let s = m.server_state;
        let mut values = vec![
            number(self.k("alltime_dl_bytes"), s.alltime_dl),
            number(self.k("alltime_ul_bytes"), s.alltime_ul),
            number(self.k("free_space_bytes"), s.free_space_on_disk),
        ];
        if let Ok(ratio) = s.global_ratio.parse::<f64>() {
            values.push(number(self.k("global_ratio"), ratio));
        }
        Ok(values)
    }

    async fn poll(&self) -> Vec<(&'static str, anyhow::Result<Values>)> {
        let (version, transfer, torrents, maindata) =
            tokio::join!(self.version(), self.transfer(), self.torrents(), self.maindata());
        vec![
            ("app/version", version),
            ("transfer/info", transfer),
            ("torrents/info", torrents),
            ("sync/maindata", maindata),
        ]
    }
}

pub struct Qbittorrent {
    name: String,
    interval: Duration,
    servers: Vec<Server>,
}

impl Qbittorrent {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(t) => crate::period::seconds(t, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        anyhow::ensure!(!settings.servers.is_empty(), "servers must list at least one server");
        anyhow::ensure!(settings.history > 0, "history must be greater than 0");
        let servers = settings
            .servers
            .into_iter()
            .map(|s| {
                let label = s.name.clone();
                Server::new(s, timeout, settings.history).with_context(|| format!("server {label:?}"))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { name: name.to_owned(), interval, servers })
    }
}

fn sum(values: &Values, servers: &[Server], metric: &str) -> f64 {
    let keys: Vec<String> = servers.iter().map(|s| s.k(metric)).collect();
    values.iter().filter(|(k, _)| keys.contains(k)).filter_map(|(_, v)| v.as_f64()).sum()
}

#[async_trait]
impl Plugin for Qbittorrent {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let polled = futures::future::join_all(self.servers.iter().map(Server::poll)).await;
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        let mut up = Vec::new();
        for (server, parts) in self.servers.iter().zip(polled) {
            let mut any = false;
            for (part, result) in parts {
                match result {
                    Ok(values) => {
                        any = true;
                        partial.ok(values);
                    }
                    Err(e) => partial.fail(&format!("{} {part}", server.key), &e),
                }
            }
            up.push((server.k("up"), any));
        }
        let mut values = partial.finish()?;
        let totals = ["torrents", "dl_speed", "up_speed", "errored"].map(|m| (m, sum(&values, &self.servers, m)));
        values.push(number("servers_up".into(), up.iter().filter(|(_, ok)| *ok).count() as f64));
        for (key, ok) in up {
            values.push(number(key, if ok { 1.0 } else { 0.0 }));
        }
        for (metric, total) in totals {
            values.push(number(metric.to_owned(), total));
        }
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const PASSWORD: &str = "qb-pass-not-real";
    const PRIVATE: [&str; 4] = [
        "Secret.Show.S01E01.1080p",
        "/downloads/private/folder",
        "https://tracker.example.invalid/announce",
        "abcdef0123",
    ];

    #[test]
    fn buckets_every_documented_state() {
        use Bucket::*;
        let cases = [
            ("downloading", Some(Downloading)),
            ("forcedDL", Some(Downloading)),
            ("metaDL", Some(Downloading)),
            ("forcedMetaDL", Some(Downloading)),
            ("uploading", Some(Seeding)),
            ("forcedUP", Some(Seeding)),
            ("stalledUP", Some(Seeding)),
            ("stalledDL", Some(Stalled)),
            ("pausedUP", Some(Paused)),
            ("pausedDL", Some(Paused)),
            ("stoppedUP", Some(Paused)),
            ("stoppedDL", Some(Paused)),
            ("queuedUP", Some(Queued)),
            ("queuedDL", Some(Queued)),
            ("checkingUP", Some(Checking)),
            ("checkingDL", Some(Checking)),
            ("checkingResumeData", Some(Checking)),
            ("error", Some(Errored)),
            ("missingFiles", Some(Errored)),
            ("allocating", None),
            ("moving", None),
            ("unknown", None),
        ];
        for (state, want) in cases {
            assert_eq!(bucket(state), want, "{state}");
        }
    }

    fn torrent(state: &str, category: &str, progress: f64, left: f64, dl: f64, up: f64) -> serde_json::Value {
        json!({
            "hash": "abcdef0123", "name": "Secret.Show.S01E01.1080p", "save_path": "/downloads/private/folder",
            "content_path": "/downloads/private/folder/Secret.Show.S01E01.1080p", "tracker": "https://tracker.example.invalid/announce",
            "magnet_uri": "magnet:?xt=urn:btih:abcdef0123", "state": state, "category": category, "tags": "",
            "progress": progress, "size": 1000, "amount_left": left, "ratio": 1.5, "num_seeds": 3, "num_leechs": 1,
            "dlspeed": dl, "upspeed": up, "eta": 8640000
        })
    }

    async fn qbit(sid: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .and(header("referer", format!("{}/", server.uri())))
            .and(header("origin", server.uri()))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("username=admin"))
            .and(body_string_contains("password=qb-pass-not-real"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("set-cookie", format!("SID={sid}; HttpOnly; SameSite=Strict; path=/"))
                    .set_body_string("Ok."),
            )
            .mount(&server)
            .await;
        mount_data(&server, sid).await;
        server
    }

    async fn mount_data(server: &MockServer, sid: &str) {
        let cookie = format!("SID={sid}");
        let ok = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
        let routes = [
            ("/api/v2/app/version", ResponseTemplate::new(200).set_body_string("v5.1.2")),
            (
                "/api/v2/transfer/info",
                ok(
                    json!({"dl_info_speed": 2048, "dl_info_data": 1_000_000, "up_info_speed": 512, "up_info_data": 300_000,
                          "dl_rate_limit": 0, "up_rate_limit": 0, "dht_nodes": 312, "connection_status": "firewalled"}),
                ),
            ),
            (
                "/api/v2/torrents/info",
                ok(json!([
                    torrent("downloading", "movies", 0.5, 500.0, 2048.0, 0.0),
                    torrent("stalledDL", "movies", 0.2, 800.0, 0.0, 0.0),
                    torrent("stalledUP", "tv", 1.0, 0.0, 0.0, 0.0),
                    torrent("uploading", "tv", 1.0, 0.0, 0.0, 512.0),
                    torrent("stoppedUP", "", 1.0, 0.0, 0.0, 0.0),
                    torrent("missingFiles", "tv", 0.9, 100.0, 0.0, 0.0),
                    torrent("queuedDL", "movies", 0.0, 1000.0, 0.0, 0.0)
                ])),
            ),
        ];
        for (route, response) in routes {
            Mock::given(method("GET"))
                .and(path(route))
                .and(header("cookie", cookie.as_str()))
                .respond_with(response)
                .mount(server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/api/v2/sync/maindata"))
            .and(query_param("rid", "0"))
            .and(header("cookie", cookie.as_str()))
            .respond_with(ok(json!({
                "rid": 1, "full_update": true, "torrents": {"abcdef0123": {"name": "Secret.Show.S01E01.1080p"}},
                "categories": {"movies": {"name": "movies", "savePath": "/downloads/private/folder"}},
                "server_state": {"alltime_dl": 9_000_000_000_i64, "alltime_ul": 4_500_000_000_i64, "global_ratio": "0.50",
                                 "free_space_on_disk": 123_456_789_000_i64, "connection_status": "firewalled"}
            })))
            .mount(server)
            .await;
    }

    fn plugin(servers: &[(&str, &MockServer)], password: &str, extra: &str) -> Qbittorrent {
        let list: Vec<String> = servers
            .iter()
            .map(|(name, s)| format!("  - {{name: {name}, url: '{}', username: admin, password: {password}}}", s.uri()))
            .collect();
        let yaml = format!("servers:\n{}\n{extra}", list.join("\n"));
        Qbittorrent::new("qbit", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Qbittorrent) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn logs_in_and_reads_every_endpoint() {
        let server = qbit("sid-one").await;
        let got = poll(&plugin(&[("nas", &server)], PASSWORD, "")).await;
        let n = |k: &str| got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} missing"));
        assert_eq!(n("nas/up"), 1.0);
        assert_eq!(got["nas/version"], Value::Text("v5.1.2".into()));
        assert_eq!(n("nas/dl_speed"), 2048.0);
        assert_eq!(n("nas/up_speed"), 512.0);
        assert_eq!(n("nas/session_dl_bytes"), 1_000_000.0);
        assert_eq!(n("nas/session_up_bytes"), 300_000.0);
        assert_eq!(n("nas/dht_nodes"), 312.0);
        assert_eq!(n("nas/connected"), 0.0);
        assert_eq!(got["nas/connection"], Value::Text("firewalled".into()));
        assert_eq!(n("nas/alltime_dl_bytes"), 9e9);
        assert_eq!(n("nas/alltime_ul_bytes"), 4.5e9);
        assert_eq!(n("nas/global_ratio"), 0.5);
        assert_eq!(n("nas/free_space_bytes"), 123_456_789_000.0);
        assert_eq!(n("nas/torrents"), 7.0);
        assert_eq!(n("nas/downloading"), 1.0);
        assert_eq!(n("nas/seeding"), 2.0);
        assert_eq!(n("nas/stalled"), 1.0);
        assert_eq!(n("nas/paused"), 1.0);
        assert_eq!(n("nas/queued"), 1.0);
        assert_eq!(n("nas/checking"), 0.0);
        assert_eq!(n("nas/errored"), 1.0);
        assert_eq!(n("nas/left_bytes"), 2400.0);
        assert_eq!(n("nas/active"), 2.0);
        assert_eq!(n("nas/category/movies/torrents"), 3.0);
        assert_eq!(n("nas/category/tv/torrents"), 3.0);
        assert!(!got.keys().any(|k| k.starts_with("nas/category//") || k == "nas/category/-/torrents"));
        assert_eq!(got["nas/dl_history"], Value::Series(vec![2048.0]));
        assert_eq!(got["nas/up_history"], Value::Series(vec![512.0]));
        assert_eq!(n("servers_up"), 1.0);
        assert_eq!(n("torrents"), 7.0);
        assert_eq!(n("dl_speed"), 2048.0);
        assert_eq!(n("up_speed"), 512.0);
        assert_eq!(n("errored"), 1.0);
        assert_eq!(n("_requests_failed"), 0.0);
        let leak = got.iter().find(|(k, v)| {
            let text = format!("{k} {}", v.as_text().unwrap_or_default());
            PRIVATE.iter().any(|p| text.contains(p))
        });
        assert!(leak.is_none(), "{leak:?}");
    }

    #[tokio::test]
    async fn the_ring_keeps_the_last_samples_oldest_first() {
        let server = qbit("sid-one").await;
        let p = plugin(&[("nas", &server)], PASSWORD, "history: 2");
        for _ in 0..3 {
            p.poll().await.unwrap();
        }
        let got = poll(&p).await;
        assert_eq!(got["nas/dl_history"], Value::Series(vec![2048.0, 2048.0]));
    }

    #[tokio::test]
    async fn a_forbidden_session_logs_in_again_once() {
        let server = MockServer::start().await;
        let login = |sid: &str| {
            Mock::given(method("POST")).and(path("/api/v2/auth/login")).respond_with(
                ResponseTemplate::new(200)
                    .insert_header("set-cookie", format!("SID={sid}; path=/"))
                    .set_body_string("Ok."),
            )
        };
        login("stale").up_to_n_times(1).expect(1).mount(&server).await;
        login("fresh").expect(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(header("cookie", "SID=stale"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;
        mount_data(&server, "fresh").await;
        let got = poll(&plugin(&[("nas", &server)], PASSWORD, "")).await;
        assert_eq!(got["nas/torrents"], Value::Number(7.0));
        assert_eq!(got["_requests_failed"], Value::Number(0.0));
    }

    #[tokio::test]
    async fn a_refused_login_fails_without_echoing_the_password() {
        for response in [ResponseTemplate::new(401), ResponseTemplate::new(200).set_body_string("Fails.")] {
            let server = MockServer::start().await;
            Mock::given(method("POST")).and(path("/api/v2/auth/login")).respond_with(response).mount(&server).await;
            let err = format!("{:#}", plugin(&[("nas", &server)], PASSWORD, "").poll().await.unwrap_err());
            assert!(err.contains("login") && !err.contains(PASSWORD), "{err}");
        }
    }

    #[tokio::test]
    async fn a_down_server_leaves_the_other() {
        let good = qbit("sid-one").await;
        let down = MockServer::start().await;
        let got = poll(&plugin(&[("nas", &good), ("seedbox", &down)], PASSWORD, "")).await;
        assert_eq!(got["nas/up"], Value::Number(1.0));
        assert_eq!(got["seedbox/up"], Value::Number(0.0));
        assert_eq!(got["servers_up"], Value::Number(1.0));
        assert_eq!(got["torrents"], Value::Number(7.0));
        assert_eq!(got["_requests_failed"], Value::Number(4.0));
    }

    #[test]
    fn defaults_and_validation() {
        let one = "servers: [{name: nas, url: 'http://qbit.example.invalid', username: u, password: p}]";
        let s: Settings = serde_yaml::from_str(one).unwrap();
        assert_eq!((s.history, s.timeout_s), (60, None));
        assert!(!s.servers[0].insecure);
        assert_eq!(Qbittorrent::new("q", s).unwrap().interval(), Duration::from_secs(30));
        let none: Settings = serde_yaml::from_str("servers: []").unwrap();
        assert!(Qbittorrent::new("q", none).is_err());
        let no_history: Settings = serde_yaml::from_str(&format!("{one}\nhistory: 0")).unwrap();
        assert!(Qbittorrent::new("q", no_history).is_err());
    }
}
