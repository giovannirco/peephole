//! Homarr's REST API (homarr-labs/homarr v2.9.1). Every route is generated from the tRPC procedures
//! listed in `packages/api/src/open-api.ts` (served by `apps/nextjs/src/app/api/[...trpc]/route.ts`);
//! the key goes in the `ApiKey` header (`packages/auth/api-key/constants.ts`) as `<id>.<token>`.
//!
//! - `GET /api/info` (`router/info.ts`): `{version}`.
//! - `GET /api/boards` (`router/board.ts` `getAllBoards`, `boardSummarySchema` in
//!   `packages/validation/src/board.ts`): `[{id, name, isPublic, creator, …}]`. Answers anonymous
//!   callers with the public boards, so a rejected key shows on `/api/info` and `/api/apps` (401).
//! - `GET /api/apps` (`router/app.ts` `all`, the `apps` table in `packages/db/schema`):
//!   `[{id, name, description, iconUrl, href, pingUrl}]`; needs the `app-modify-all` permission (403 without).
//!
//! Only names, `isPublic` and whether `pingUrl` is set are read: no URLs, creators or permissions.
//! The REST API lists no integrations (they are tRPC-only) and no app status (pings are a tRPC
//! subscription), so neither is emitted.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the struct holds the instance's API key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Homarr base URL, e.g. `https://homarr.example.invalid`.
    pub url: String,
    /// Sent as `ApiKey`; never logged.
    pub api_key: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Accept a self-signed certificate.
    #[serde(default)]
    pub insecure: bool,
}

fn default_interval() -> u64 {
    120
}

#[derive(Deserialize)]
struct Info {
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Board {
    name: String,
    is_public: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct App {
    #[serde(default)]
    ping_url: Option<String>,
}

/// The key was refused: the whole poll fails instead of showing only public boards.
#[derive(Debug)]
struct Rejected(String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GET {}: HTTP 401, the API key was rejected", self.0)
    }
}

impl std::error::Error for Rejected {}

type Values = Vec<(String, Value)>;

fn number(n: usize) -> Value {
    Value::Number(n as f64)
}

pub struct Homarr {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    base: reqwest::Url,
}

impl Homarr {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(s) => crate::period::seconds(s, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        let headers = [("ApiKey".to_owned(), settings.api_key)].into();
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &headers, None, settings.insecure)?,
            base: crate::http::base_url(&settings.url)?,
        })
    }

    async fn json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> anyhow::Result<T> {
        let url = crate::http::join(&self.base, path);
        let path = url.path().to_owned();
        let response = self.client.get(url).send().await.with_context(|| format!("GET {path}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Rejected(path).into());
        }
        anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}");
        response.json().await.with_context(|| format!("GET {path}: unexpected body"))
    }

    async fn info(&self) -> anyhow::Result<Values> {
        let info: Info = self.json("api/info").await?;
        Ok(vec![("version".into(), Value::Text(info.version))])
    }

    async fn boards(&self) -> anyhow::Result<Values> {
        let boards: Vec<Board> = self.json("api/boards").await?;
        let mut values = vec![
            ("boards".to_owned(), number(boards.len())),
            ("boards_public".to_owned(), number(boards.iter().filter(|b| b.is_public).count())),
        ];
        for board in &boards {
            let key = crate::key::sanitize(&board.name);
            values.push((format!("board/{key}/public"), number(board.is_public as usize)));
        }
        Ok(values)
    }

    async fn apps(&self) -> anyhow::Result<Values> {
        let apps: Vec<App> = self.json("api/apps").await?;
        let pinged = apps.iter().filter(|a| a.ping_url.as_deref().is_some_and(|u| !u.is_empty())).count();
        Ok(vec![("apps".to_owned(), number(apps.len())), ("apps_with_ping".to_owned(), number(pinged))])
    }
}

#[async_trait]
impl Plugin for Homarr {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let (info, boards, apps) = tokio::join!(self.info(), self.boards(), self.apps());
        let mut partial = crate::partial::Partial::new(&self.name, "endpoints");
        for (endpoint, result) in [("info", info), ("boards", boards), ("apps", apps)] {
            match result {
                Ok(values) => partial.ok(values),
                Err(e) if e.is::<Rejected>() => return Err(e),
                Err(e) => partial.fail(endpoint, &e),
            }
        }
        partial.finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const KEY: &str = "keyid01.not-a-real-token";

    fn boards() -> serde_json::Value {
        json!([
            {
                "id": "b1", "name": "default", "logoImageUrl": null, "isPublic": true,
                "creator": {"id": "u1", "name": "admin", "image": null},
                "isHome": true, "isMobileHome": false, "userPermissions": [], "groupPermissions": []
            },
            {
                "id": "b2", "name": "Lab Ops", "logoImageUrl": "https://img.example.invalid/logo.png", "isPublic": false,
                "creator": {"id": "u2", "name": "gio", "image": "https://img.example.invalid/gio.png"},
                "isHome": false, "isMobileHome": false,
                "userPermissions": [{"permission": "modify"}], "groupPermissions": []
            }
        ])
    }

    fn apps() -> serde_json::Value {
        json!([
            {
                "id": "a1", "name": "Grafana", "description": "Dashboards",
                "iconUrl": "https://cdn.example.invalid/grafana.svg",
                "href": "https://grafana.example.invalid", "pingUrl": "https://grafana.example.invalid/api/health"
            },
            {
                "id": "a2", "name": "Jellyfin", "description": null,
                "iconUrl": "https://cdn.example.invalid/jellyfin.svg", "href": "https://jf.example.invalid", "pingUrl": null
            },
            {"id": "a3", "name": "Notes", "description": null, "iconUrl": "https://cdn.example.invalid/notes.svg", "href": null, "pingUrl": ""}
        ])
    }

    async fn mount(server: &MockServer, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("apikey", KEY))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn instance(apps_response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        mount(&server, "/api/info", ResponseTemplate::new(200).set_body_json(json!({"version": "2.9.1"}))).await;
        mount(&server, "/api/boards", ResponseTemplate::new(200).set_body_json(boards())).await;
        mount(&server, "/api/apps", apps_response).await;
        server
    }

    fn plugin(server: &MockServer, key: &str) -> Homarr {
        let yaml = format!("url: {}\napi_key: {key}", server.uri());
        Homarr::new("homarr", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Homarr) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn reads_version_boards_and_apps() {
        let server = instance(ResponseTemplate::new(200).set_body_json(apps())).await;
        let got = poll(&plugin(&server, KEY)).await;
        let n = |k: &str| got.get(k).and_then(Value::as_f64).unwrap_or_else(|| panic!("{k} missing in {got:?}"));
        assert_eq!(got["version"], Value::Text("2.9.1".into()));
        assert_eq!(n("boards"), 2.0);
        assert_eq!(n("boards_public"), 1.0);
        assert_eq!(n("board/default/public"), 1.0);
        assert_eq!(n("board/Lab-Ops/public"), 0.0);
        assert_eq!(n("apps"), 3.0);
        assert_eq!(n("apps_with_ping"), 1.0);
        assert_eq!(n("_endpoints_ok"), 3.0);
        assert_eq!(n("_endpoints_failed"), 0.0);
    }

    #[tokio::test]
    async fn never_emits_urls_or_people() {
        let server = instance(ResponseTemplate::new(200).set_body_json(apps())).await;
        let got = poll(&plugin(&server, KEY)).await;
        for (key, value) in &got {
            assert!(!key.contains("example.invalid") && !key.contains("gio") && !key.contains("admin"), "{key}");
            if let Value::Text(text) = value {
                assert!(!text.contains("example.invalid") && text != "gio" && text != "admin", "{key} = {text}");
            }
        }
    }

    #[tokio::test]
    async fn one_failing_endpoint_leaves_the_others() {
        let forbidden =
            ResponseTemplate::new(403).set_body_json(json!({"message": "Permission denied", "code": "FORBIDDEN"}));
        let server = instance(forbidden).await;
        let got = poll(&plugin(&server, KEY)).await;
        assert_eq!(got["boards"], Value::Number(2.0));
        assert!(!got.contains_key("apps"));
        assert_eq!(got["_endpoints_failed"], Value::Number(1.0));
    }

    #[tokio::test]
    async fn a_rejected_key_fails_the_poll_without_echoing_it() {
        let server = MockServer::start().await;
        let unauthorized =
            ResponseTemplate::new(401).set_body_json(json!({"message": "UNAUTHORIZED", "code": "UNAUTHORIZED"}));
        Mock::given(path("/api/info")).respond_with(unauthorized.clone()).mount(&server).await;
        Mock::given(path("/api/apps")).respond_with(unauthorized).mount(&server).await;
        Mock::given(path("/api/boards"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let err = format!("{:#}", plugin(&server, "keyid02.wrong-token-value").poll().await.unwrap_err());
        assert!(err.contains("401") && !err.contains("wrong-token-value") && !err.contains("keyid02"), "{err}");
    }

    #[tokio::test]
    async fn every_endpoint_failing_is_an_error() {
        let server = MockServer::start().await;
        let err = plugin(&server, KEY).poll().await.unwrap_err().to_string();
        assert!(err.contains("/api/info") && err.contains("404"), "{err}");
    }

    #[test]
    fn defaults() {
        let s: Settings = serde_yaml::from_str("{url: 'https://homarr.example.invalid', api_key: k}").unwrap();
        assert!(!s.insecure && s.timeout_s.is_none());
        assert_eq!(Homarr::new("h", s).unwrap().interval(), Duration::from_secs(120));
        assert!(serde_yaml::from_str::<Settings>("{url: 'https://homarr.example.invalid'}").is_err());
        assert!(serde_yaml::from_str::<Settings>("{url: u, api_key: k, apikey: k}").is_err());
    }
}
