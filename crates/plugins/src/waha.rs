//! WAHA (WhatsApp HTTP API), verified against the source at
//! <https://github.com/devlikeapro/waha> commit 0858b4f0 (2026-10-14):
//! `src/api/server.controller.ts`, `src/api/sessions.controller.ts`,
//! `src/structures/server.dto.ts`, `environment.dto.ts`, `sessions.dto.ts`, `enums.dto.ts`,
//! `src/core/manager.core.ts` and `src/core/auth/HeaderOrQueryApiKeyStrategy.ts`.
//! Every request sends `X-Api-Key`.
//!
//! - `GET /api/server/version` → `{version, engine, tier, browser, platform, worker}`.
//! - `GET /api/server/status` → `{startTimestamp, uptime, worker}`, both in milliseconds.
//! - `GET /api/sessions?all=true` → `[{name, status, config, me, assignedWorker, presence, timestamps}]`,
//!   `status` one of STOPPED, STARTING, SCAN_QR_CODE, PASSKEY_REQUIRED,
//!   PASSKEY_CONFIRMATION_REQUIRED, WORKING, FAILED. The list carries no engine (only
//!   `GET /api/sessions/{name}` does), so the engine comes from the server version.
//!
//! Only `name` and `status` are read from a session: `me` (phone number, push name) and
//! `config` (proxy credentials, webhooks) are never deserialized.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the servers hold API keys.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub servers: Vec<Server>,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// No `Debug`: holds the API key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Key prefix for this server's values.
    pub name: String,
    /// Base URL, e.g. `http://waha.example.invalid:3000`.
    pub url: String,
    /// Sent as `X-Api-Key`; never logged.
    pub api_key: String,
    #[serde(default)]
    pub insecure: bool,
}

fn default_interval() -> u64 {
    60
}

#[derive(Deserialize)]
struct Version {
    version: String,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    tier: Option<String>,
}

#[derive(Deserialize)]
struct Status {
    uptime: f64,
}

#[derive(Deserialize)]
struct Session {
    name: String,
    status: String,
}

impl Session {
    fn working(&self) -> bool {
        self.status == "WORKING"
    }
    /// Needs a human or a restart: anything that is neither working nor on its way there.
    fn broken(&self) -> bool {
        !matches!(self.status.as_str(), "WORKING" | "STARTING")
    }
}

type Values = Vec<(String, Value)>;

struct Target {
    key: String,
    client: reqwest::Client,
    base: reqwest::Url,
}

impl Target {
    async fn json<T: for<'de> Deserialize<'de>>(&self, url: reqwest::Url) -> anyhow::Result<T> {
        let path = url.path().to_owned();
        crate::http::get(&self.client, url).await?.json().await.map_err(|e| anyhow::anyhow!("GET {path}: {e}"))
    }

    async fn version(&self) -> anyhow::Result<Version> {
        self.json(crate::http::join(&self.base, "api/server/version")).await
    }

    async fn status(&self) -> anyhow::Result<Status> {
        self.json(crate::http::join(&self.base, "api/server/status")).await
    }

    async fn sessions(&self) -> anyhow::Result<Vec<Session>> {
        let mut url = crate::http::join(&self.base, "api/sessions");
        url.query_pairs_mut().append_pair("all", "true");
        self.json(url).await
    }
}

pub struct Waha {
    name: String,
    interval: Duration,
    targets: Vec<Target>,
}

impl Waha {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = settings
            .timeout_s
            .map(|t| crate::period::seconds(t, "timeout_s"))
            .transpose()?
            .unwrap_or(crate::http::DEFAULT_TIMEOUT.min(interval));
        anyhow::ensure!(!settings.servers.is_empty(), "servers must list at least one server");
        let mut seen = BTreeSet::new();
        let mut targets = Vec::new();
        for server in settings.servers {
            let key = crate::key::sanitize(&server.name);
            anyhow::ensure!(seen.insert(key.clone()), "server name {:?} is used twice", server.name);
            let headers = [("X-Api-Key".to_owned(), server.api_key)].into();
            targets.push(Target {
                client: crate::http::client(timeout, &headers, None, server.insecure)
                    .map_err(|e| anyhow::anyhow!("server {key}: {e:#}"))?,
                base: crate::http::base_url(&server.url)?,
                key,
            });
        }
        Ok(Self { name: name.to_owned(), interval, targets })
    }
}

fn version_values(s: &str, v: Version) -> Values {
    let mut out = vec![(format!("{s}/up"), Value::Number(1.0)), (format!("{s}/version"), Value::Text(v.version))];
    if let Some(engine) = v.engine {
        out.push((format!("{s}/engine"), Value::Text(engine)));
    }
    if let Some(tier) = v.tier {
        out.push((format!("{s}/tier"), Value::Text(tier)));
    }
    out
}

fn session_values(s: &str, sessions: &[Session]) -> Values {
    let working = sessions.iter().filter(|x| x.working()).count();
    let mut out = vec![
        (format!("{s}/sessions_total"), Value::Number(sessions.len() as f64)),
        (format!("{s}/sessions_working"), Value::Number(working as f64)),
    ];
    for session in sessions {
        let key = crate::key::sanitize(&session.name);
        out.push((format!("{s}/session/{key}/working"), Value::Number(if session.working() { 1.0 } else { 0.0 })));
        out.push((format!("{s}/session/{key}/status"), Value::Text(session.status.clone())));
    }
    out
}

#[async_trait]
impl Plugin for Waha {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let polled = futures::future::join_all(
            self.targets.iter().map(|t| async move { tokio::join!(t.version(), t.status(), t.sessions()) }),
        )
        .await;
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        let (mut up, mut total, mut working) = (0, 0, 0);
        let mut broken = Vec::new();
        let mut down = Vec::new();
        for (target, (version, status, sessions)) in self.targets.iter().zip(polled) {
            let s = &target.key;
            match version {
                Ok(v) => {
                    up += 1;
                    partial.ok(version_values(s, v));
                }
                Err(e) => {
                    down.push((format!("{s}/up"), Value::Number(0.0)));
                    partial.fail(&format!("{s} version"), &e);
                }
            }
            match status {
                Ok(st) => partial.ok([(format!("{s}/uptime"), Value::Number((st.uptime / 1000.0).floor()))]),
                Err(e) => partial.fail(&format!("{s} status"), &e),
            }
            match sessions {
                Ok(list) => {
                    total += list.len();
                    working += list.iter().filter(|x| x.working()).count();
                    broken.extend(
                        list.iter().filter(|x| x.broken()).map(|x| format!("{s}/{}", crate::key::sanitize(&x.name))),
                    );
                    partial.ok(session_values(s, &list));
                }
                Err(e) => partial.fail(&format!("{s} sessions"), &e),
            }
        }
        broken.sort();
        let mut out = partial.finish()?;
        out.extend(down);
        out.extend([
            ("servers_total".into(), Value::Number(self.targets.len() as f64)),
            ("servers_up".into(), Value::Number(up as f64)),
            ("sessions_total".into(), Value::Number(total as f64)),
            ("sessions_working".into(), Value::Number(working as f64)),
            ("sessions_broken".into(), Value::Number(broken.len() as f64)),
            ("broken_list".into(), Value::Text(broken.join(", "))),
        ]);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const KEY_A: &str = "waha-key-a-not-real";
    const KEY_B: &str = "waha-key-b-not-real";
    const PHONE: &str = "5511999990000";

    fn session(name: &str, status: &str) -> serde_json::Value {
        json!({
            "name": name,
            "status": status,
            "config": {
                "proxy": {"server": "proxy.example.invalid:3128", "username": "proxyuser", "password": "proxypass"},
                "webhooks": [{"url": "https://hooks.example.invalid/wa?token=hooktoken", "events": ["message"]}],
                "metadata": {"user.email": "someone@example.invalid"}
            },
            "me": {"id": format!("{PHONE}@c.us"), "lid": "123123@lid", "jid": format!("{PHONE}:12@s.whatsapp.net"), "pushName": "Pushy McName"},
            "assignedWorker": "worker-1",
            "presence": "online",
            "timestamps": {"activity": 1761400000000_i64}
        })
    }

    async fn mount(server: &MockServer, key: &str, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(route.to_owned()))
            .and(header("x-api-key", key))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn waha(key: &str, sessions: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        let ok = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
        mount(
            &server,
            key,
            "/api/server/version",
            ok(json!({"version": "2026.10.2", "engine": "GOWS", "tier": "PLUS", "browser": null, "platform": "linux/x64", "worker": {"id": "worker-1"}})),
        )
        .await;
        mount(
            &server,
            key,
            "/api/server/status",
            ok(json!({"startTimestamp": 1761390000000_i64, "uptime": 3_725_400, "worker": {"id": "worker-1"}})),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/api/sessions"))
            .and(query_param("all", "true"))
            .and(header("x-api-key", key))
            .respond_with(ok(sessions))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"message": "Unauthorized", "statusCode": 401})),
            )
            .mount(&server)
            .await;
        server
    }

    fn every_status() -> serde_json::Value {
        json!([
            session("bitops", "WORKING"),
            session("p8571", "SCAN_QR_CODE"),
            session("booting", "STARTING"),
            session("old", "STOPPED"),
            session("crashed", "FAILED"),
            session("passkey", "PASSKEY_REQUIRED"),
            session("confirm", "PASSKEY_CONFIRMATION_REQUIRED")
        ])
    }

    fn plugin(servers: &[(&str, String, &str)]) -> Waha {
        let list: Vec<String> = servers
            .iter()
            .map(|(name, url, key)| format!("  - {{name: {name}, url: '{url}', api_key: {key}}}"))
            .collect();
        let yaml = format!("servers:\n{}", list.join("\n"));
        Waha::new("waha", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Waha) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    fn n(got: &BTreeMap<String, Value>, key: &str) -> f64 {
        got.get(key).and_then(Value::as_f64).unwrap_or_else(|| panic!("{key} missing"))
    }

    #[tokio::test]
    async fn reports_each_server_and_its_sessions() {
        let a = waha(KEY_A, every_status()).await;
        let got = poll(&plugin(&[("bitops", a.uri(), KEY_A)])).await;
        assert_eq!(n(&got, "bitops/up"), 1.0);
        assert_eq!(got["bitops/version"], Value::Text("2026.10.2".into()));
        assert_eq!(got["bitops/engine"], Value::Text("GOWS".into()));
        assert_eq!(got["bitops/tier"], Value::Text("PLUS".into()));
        assert_eq!(n(&got, "bitops/uptime"), 3725.0);
        assert_eq!(n(&got, "bitops/sessions_total"), 7.0);
        assert_eq!(n(&got, "bitops/sessions_working"), 1.0);
        assert_eq!(n(&got, "bitops/session/bitops/working"), 1.0);
        assert_eq!(n(&got, "bitops/session/p8571/working"), 0.0);
        assert_eq!(got["bitops/session/p8571/status"], Value::Text("SCAN_QR_CODE".into()));
        assert_eq!(got["bitops/session/confirm/status"], Value::Text("PASSKEY_CONFIRMATION_REQUIRED".into()));
        assert_eq!(n(&got, "sessions_broken"), 5.0);
        assert_eq!(
            got["broken_list"],
            Value::Text("bitops/confirm, bitops/crashed, bitops/old, bitops/p8571, bitops/passkey".into())
        );
    }

    #[tokio::test]
    async fn a_down_server_is_counted_and_the_other_kept() {
        let a = waha(KEY_A, every_status()).await;
        let b = waha(KEY_B, json!([session("default", "WORKING")])).await;
        let down = "http://127.0.0.1:9".to_owned();
        let got = poll(&plugin(&[("a", a.uri(), KEY_A), ("b", b.uri(), KEY_B), ("gone", down, KEY_B)])).await;
        assert_eq!(n(&got, "servers_total"), 3.0);
        assert_eq!(n(&got, "servers_up"), 2.0);
        assert_eq!(n(&got, "gone/up"), 0.0);
        assert!(!got.contains_key("gone/sessions_total"));
        assert_eq!(n(&got, "sessions_total"), 8.0);
        assert_eq!(n(&got, "sessions_working"), 2.0);
        assert_eq!(n(&got, "b/session/default/working"), 1.0);
        assert_eq!(n(&got, "_requests_ok"), 6.0);
        assert_eq!(n(&got, "_requests_failed"), 3.0);
    }

    #[tokio::test]
    async fn nothing_broken_is_an_empty_list() {
        let b = waha(KEY_B, json!([session("default", "WORKING"), session("next", "STARTING")])).await;
        let got = poll(&plugin(&[("b", b.uri(), KEY_B)])).await;
        assert_eq!(n(&got, "sessions_broken"), 0.0);
        assert_eq!(got["broken_list"], Value::Text(String::new()));
    }

    #[tokio::test]
    async fn no_account_detail_reaches_a_key_or_value() {
        let a = waha(KEY_A, every_status()).await;
        let got = poll(&plugin(&[("a", a.uri(), KEY_A)])).await;
        for (key, value) in &got {
            let text = format!("{key} {value:?}");
            for forbidden in [PHONE, "@c.us", "@lid", "Pushy", "proxyuser", "proxypass", "hooktoken", "someone@", KEY_A]
            {
                assert!(!text.contains(forbidden), "{forbidden} in {text}");
            }
        }
    }

    #[tokio::test]
    async fn a_wrong_key_fails_the_poll_without_echoing_it() {
        let a = waha(KEY_A, every_status()).await;
        let err = format!("{:#}", plugin(&[("a", a.uri(), "wrong-key-value")]).poll().await.unwrap_err());
        assert!(err.contains("401") && !err.contains("wrong-key-value"), "{err}");
    }

    #[test]
    fn servers_must_be_named_once_and_listed() {
        let none: Settings = serde_yaml::from_str("servers: []").unwrap();
        assert!(Waha::new("w", none).is_err());
        let twice: Settings = serde_yaml::from_str(
            "servers: [{name: a, url: 'http://w.example.invalid', api_key: k}, {name: a, url: 'http://v.example.invalid', api_key: k}]",
        )
        .unwrap();
        assert!(Waha::new("w", twice).is_err());
    }

    #[test]
    fn defaults() {
        let s: Settings =
            serde_yaml::from_str("servers: [{name: a, url: 'http://w.example.invalid', api_key: k}]").unwrap();
        assert!(!s.servers[0].insecure && s.timeout_s.is_none());
        assert_eq!(Waha::new("w", s).unwrap().interval(), Duration::from_secs(60));
    }
}
