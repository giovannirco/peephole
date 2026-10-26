//! CLIProxyAPI management API, verified against the source of v8.0.38 (deployed) and
//! v8.0.43 (`internal/api/server_management.go`, `internal/api/handlers/management/
//! auth_files.go` and `handler.go`, `sdk/cliproxy/auth/types.go`) and `docs/management-api-v8.md`.
//!
//! One request: `GET /v0/management/auth-files` with `Authorization: Bearer <management key>`
//! returns `{"observed_at", "files": [...]}`; each file carries `provider`, `disabled`,
//! `unavailable`, lifetime `success`/`failed` counters and `recent_requests`, twenty
//! 10-minute buckets oldest first. The running version is the `X-CPA-VERSION` header.
//! v8 has no aggregate usage endpoint: `usage-queue` pops records (destructive),
//! `api-key-usage` keys its JSON by API key and `latest-version` asks GitHub, so none is read.
//! Five wrong keys ban the client address for 30 minutes.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

const AUTH_FILES: &str = "v0/management/auth-files";

/// No `Debug`: the struct holds the management key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Base URL, e.g. `http://cliproxyapi.example.invalid:8317`.
    pub url: String,
    /// Sent as `Authorization: Bearer`; never logged.
    pub management_key: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

fn default_interval() -> u64 {
    60
}

/// The only fields read from an entry; e-mails, names, paths and labels are never deserialized.
#[derive(Deserialize)]
struct AuthFile {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    unavailable: bool,
    #[serde(default)]
    success: f64,
    #[serde(default)]
    failed: f64,
    #[serde(default)]
    recent_requests: Vec<Bucket>,
}

#[derive(Deserialize)]
struct Bucket {
    success: f64,
    failed: f64,
}

#[derive(Deserialize)]
struct AuthFiles {
    files: Vec<AuthFile>,
}

type Values = Vec<(String, Value)>;

pub struct Cliproxyapi {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    url: reqwest::Url,
}

impl Cliproxyapi {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = settings
            .timeout_s
            .map(|t| crate::period::seconds(t, "timeout_s"))
            .transpose()?
            .unwrap_or(crate::http::DEFAULT_TIMEOUT.min(interval));
        let base = crate::http::base_url(&settings.url)?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &Default::default(), Some(&settings.management_key), false)?,
            url: crate::http::join(&base, AUTH_FILES),
        })
    }
}

fn provider_key(raw: &str) -> String {
    let trimmed = raw.trim().to_lowercase();
    if trimmed.is_empty() { "unknown".to_owned() } else { crate::key::sanitize(&trimmed) }
}

fn values(files: &[AuthFile]) -> Values {
    let success: f64 = files.iter().map(|f| f.success).sum();
    let failed: f64 = files.iter().map(|f| f.failed).sum();
    let total = success + failed;
    let mut out: Values = vec![
        ("requests_total".into(), Value::Number(total)),
        ("requests_success".into(), Value::Number(success)),
        ("requests_failed".into(), Value::Number(failed)),
    ];
    if total > 0.0 {
        out.push(("success_ratio".into(), Value::Number(success / total)));
    }
    let width = files.iter().map(|f| f.recent_requests.len()).max().unwrap_or(0);
    let mut recent = vec![0.0; width];
    for file in files {
        let offset = width - file.recent_requests.len();
        for (slot, bucket) in recent[offset..].iter_mut().zip(&file.recent_requests) {
            *slot += bucket.success + bucket.failed;
        }
    }
    out.push(("requests_recent".into(), Value::Series(recent)));
    let mut providers: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for file in files {
        let entry = providers.entry(provider_key(&file.provider)).or_default();
        entry.0 += file.success + file.failed;
        entry.1 += 1.0;
    }
    for (provider, (requests, accounts)) in providers {
        out.push((format!("provider/{provider}/requests"), Value::Number(requests)));
        out.push((format!("provider/{provider}/accounts"), Value::Number(accounts)));
    }
    let count = |pick: fn(&AuthFile) -> bool| Value::Number(files.iter().filter(|f| pick(f)).count() as f64);
    out.push(("accounts_total".into(), Value::Number(files.len() as f64)));
    out.push(("accounts_available".into(), count(|f| !f.disabled && !f.unavailable)));
    out.push(("accounts_disabled".into(), count(|f| f.disabled)));
    out.push(("accounts_unavailable".into(), count(|f| !f.disabled && f.unavailable)));
    out
}

#[async_trait]
impl Plugin for Cliproxyapi {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let response = crate::http::get(&self.client, self.url.clone()).await?;
        let version = response.headers().get("x-cpa-version").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let body: AuthFiles = response.json().await.map_err(|e| anyhow::anyhow!("GET {}: {e}", self.url.path()))?;
        let mut out = values(&body.files);
        if let Some(version) = version {
            out.push(("version".into(), Value::Text(version)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const KEY: &str = "mgmt-key-not-real";
    const EMAIL: &str = "someone@example.invalid";

    fn buckets(counts: &[(i64, i64)]) -> serde_json::Value {
        let mut out: Vec<serde_json::Value> = (0..20 - counts.len())
            .map(|i| json!({"time": format!("{:02}:00-{:02}:10", i % 24, i % 24), "success": 0, "failed": 0}))
            .collect();
        out.extend(counts.iter().map(|(s, f)| json!({"time": "13:50-14:00", "success": s, "failed": f})));
        json!(out)
    }

    fn auth_files() -> serde_json::Value {
        json!({
            "observed_at": "2026-10-25T14:03:00Z",
            "files": [
                {
                    "id": format!("claude-{EMAIL}.json"), "auth_index": 0, "name": format!("claude-{EMAIL}.json"),
                    "type": "claude", "provider": "claude", "label": EMAIL, "status": "active", "status_message": "",
                    "disabled": false, "unavailable": false, "runtime_only": false, "source": "file", "size": 812,
                    "success": 90, "failed": 10, "recent_requests": buckets(&[(3, 1), (5, 0)]),
                    "quota": {"signals": {}}, "email": EMAIL, "account_type": "oauth", "account": EMAIL,
                    "path": format!("/root/.cli-proxy-api/claude-{EMAIL}.json"), "cooldowns": null,
                    "created_at": "2026-10-01T10:00:00Z", "updated_at": "2026-10-25T13:00:00Z"
                },
                {
                    "id": "codex-other@example.invalid.json", "auth_index": 1, "name": "codex-other@example.invalid.json",
                    "type": "codex", "provider": "codex", "label": "other@example.invalid", "status": "error",
                    "status_message": "token expired", "disabled": false, "unavailable": true, "runtime_only": false,
                    "source": "file", "size": 640, "success": 40, "failed": 0, "recent_requests": buckets(&[(2, 0)]),
                    "quota": {"signals": {}}, "email": "other@example.invalid", "cooldowns": null
                },
                {
                    "id": "codex-third@example.invalid.json", "auth_index": 2, "name": "codex-third@example.invalid.json",
                    "type": "codex", "provider": "codex", "label": "", "status": "disabled", "status_message": "",
                    "disabled": true, "unavailable": false, "runtime_only": false, "source": "file", "size": 640,
                    "success": 0, "failed": 0, "recent_requests": buckets(&[]), "quota": {"signals": {}}, "cooldowns": null
                },
                {
                    "id": "xai-key", "auth_index": 3, "name": "xai-key", "type": "", "provider": "", "label": "",
                    "status": "active", "status_message": "", "disabled": false, "unavailable": false,
                    "runtime_only": true, "source": "memory", "size": 0, "success": 10, "failed": 0,
                    "recent_requests": buckets(&[]), "quota": {"signals": {}}, "account_type": "api_key",
                    "account": "xai-secret-value", "cooldowns": null
                }
            ]
        })
    }

    async fn server(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v0/management/auth-files"))
            .and(header("authorization", format!("Bearer {KEY}").as_str()))
            .respond_with(ResponseTemplate::new(200).insert_header("X-CPA-VERSION", "v8.0.38").set_body_json(body))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v0/management/auth-files"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": "invalid management key"})))
            .mount(&server)
            .await;
        server
    }

    fn plugin(server: &MockServer, key: &str) -> Cliproxyapi {
        let yaml = format!("url: {}\nmanagement_key: {key}", server.uri());
        Cliproxyapi::new("cpa", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Cliproxyapi) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn sums_requests_and_counts_accounts_per_provider() {
        let server = server(auth_files()).await;
        let got = poll(&plugin(&server, KEY)).await;
        let n = |k: &str| got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} missing"));
        assert_eq!(n("requests_total"), 150.0);
        assert_eq!(n("requests_success"), 140.0);
        assert_eq!(n("requests_failed"), 10.0);
        assert!((n("success_ratio") - 140.0 / 150.0).abs() < 1e-9);
        assert_eq!(n("provider/claude/requests"), 100.0);
        assert_eq!(n("provider/codex/requests"), 40.0);
        assert_eq!(n("provider/unknown/requests"), 10.0);
        assert_eq!(n("provider/claude/accounts"), 1.0);
        assert_eq!(n("provider/codex/accounts"), 2.0);
        assert_eq!(n("accounts_total"), 4.0);
        assert_eq!(n("accounts_available"), 2.0);
        assert_eq!(n("accounts_disabled"), 1.0);
        assert_eq!(n("accounts_unavailable"), 1.0);
        assert_eq!(got["version"], Value::Text("v8.0.38".into()));
    }

    #[tokio::test]
    async fn recent_requests_are_summed_per_bucket_oldest_first() {
        let server = server(auth_files()).await;
        let got = poll(&plugin(&server, KEY)).await;
        let series = got["requests_recent"].as_series().unwrap();
        assert_eq!(series.len(), 20);
        assert!(series[..18].iter().all(|v| *v == 0.0));
        assert_eq!(series[18], 4.0);
        assert_eq!(series[19], 7.0);
    }

    #[tokio::test]
    async fn no_account_name_or_credential_reaches_a_key_or_value() {
        let server = server(auth_files()).await;
        let got = poll(&plugin(&server, KEY)).await;
        for (key, value) in &got {
            let text = format!("{key} {value:?}");
            assert!(!text.contains('@') && !text.contains("xai-secret-value") && !text.contains(":00-"), "{text}");
        }
    }

    #[tokio::test]
    async fn no_traffic_means_no_ratio() {
        let server = server(json!({"observed_at": "2026-10-25T14:03:00Z", "files": []})).await;
        let got = poll(&plugin(&server, KEY)).await;
        assert_eq!(got["requests_total"], Value::Number(0.0));
        assert_eq!(got["accounts_total"], Value::Number(0.0));
        assert!(!got.contains_key("success_ratio"));
    }

    #[tokio::test]
    async fn a_wrong_key_fails_the_poll_without_echoing_it() {
        let server = server(auth_files()).await;
        let err = format!("{:#}", plugin(&server, "wrong-key-value").poll().await.unwrap_err());
        assert!(err.contains("401") && err.contains("auth-files") && !err.contains("wrong-key-value"), "{err}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_the_list_fails_the_poll() {
        let server = server(json!({"error": "handler not initialized"})).await;
        assert!(plugin(&server, KEY).poll().await.is_err());
    }

    #[test]
    fn defaults() {
        let s: Settings = serde_yaml::from_str("{url: 'http://cpa.example.invalid:8317', management_key: k}").unwrap();
        assert_eq!(s.interval_s, 60);
        assert!(s.timeout_s.is_none());
        assert_eq!(Cliproxyapi::new("c", s).unwrap().interval(), Duration::from_secs(60));
        let zero: Settings =
            serde_yaml::from_str("{url: 'http://c.example.invalid', management_key: k, timeout_s: 0}").unwrap();
        assert!(Cliproxyapi::new("c", zero).is_err());
    }
}
