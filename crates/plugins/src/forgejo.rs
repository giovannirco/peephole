//! Forgejo / Gitea instance summary through API v1.
//!
//! Endpoints, checked on 2026-10-25 against Codeberg's published Swagger
//! (`https://codeberg.org/swagger.v1.json`, Forgejo API `16.0.0-dev+gitea-1.22.0`), live
//! Codeberg responses, and <https://forgejo.org/docs/latest/user/api-usage/>:
//! - `GET /api/v1/version` → `{"version"}`.
//! - `GET /api/v1/repos/search?limit=&sort=updated&order=desc` → `{"ok", "data": [Repository]}`;
//!   `size` is in KiB (`int(repo.Size / 1024)` in Forgejo's `services/convert/repository.go`)
//!   and `updated_at` carries the server's offset.
//! - `GET /api/v1/repos/issues/search?state=open&type=issues|pulls&limit=1` → the total in the
//!   `X-Total-Count` header.
//! - `GET /api/v1/notifications/new` → `{"new"}`.
//! - `GET /api/v1/repos/{owner}/{repo}/actions/tasks?status=failure` →
//!   `{"total_count", "workflow_runs": [ActionTask]}`; a 404 (Actions off or an older
//!   server) is not a failure.
//!
//! The token is sent as `Authorization: token <token>`. The token owner's login and e-mail are
//! never read.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the struct holds the API token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Instance base URL, e.g. `https://forgejo.example.invalid`.
    pub url: String,
    pub token: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Accept a self-signed certificate.
    #[serde(default)]
    pub insecure: bool,
    /// Page size of the repository search.
    #[serde(default = "default_repos_max")]
    pub repos_max: u32,
    /// How many of the most recently updated repositories get `repo/<name>/…` keys.
    #[serde(default = "default_top")]
    pub top: usize,
    /// Count failed Actions runs of the `top` repositories in the last 24 h.
    #[serde(default)]
    pub actions: bool,
}

fn default_interval() -> u64 {
    300
}
fn default_repos_max() -> u32 {
    50
}
fn default_top() -> usize {
    5
}

const DAY_S: i64 = 86_400;
/// Failed runs read per repository; more than this in a day still reads as this many.
const RUNS_PAGE: u32 = 50;

#[derive(Deserialize)]
struct ServerVersion {
    version: String,
}

#[derive(Deserialize)]
struct SearchResults {
    ok: bool,
    data: Vec<Repository>,
}

#[derive(Deserialize)]
struct Repository {
    name: String,
    full_name: String,
    private: bool,
    archived: bool,
    stars_count: f64,
    forks_count: f64,
    open_issues_count: f64,
    open_pr_counter: f64,
    size: f64,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct NotificationCount {
    new: f64,
}

#[derive(Deserialize)]
struct ActionTaskResponse {
    workflow_runs: Vec<ActionTask>,
}

#[derive(Deserialize)]
struct ActionTask {
    status: String,
    created_at: DateTime<Utc>,
}

type Values = Vec<(String, Value)>;

pub struct Forgejo {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    api: reqwest::Url,
    repos_max: u32,
    top: usize,
    actions: bool,
}

impl Forgejo {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = crate::period::timeout(settings.timeout_s, interval)?;
        anyhow::ensure!(settings.repos_max > 0, "repos_max must be greater than 0");
        let headers = [("Authorization".to_owned(), format!("token {}", settings.token))].into();
        let base = crate::http::base_url(&settings.url)?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &headers, None, settings.insecure)?,
            api: crate::http::join(&base, "api/v1"),
            repos_max: settings.repos_max,
            top: settings.top,
            actions: settings.actions,
        })
    }

    fn url(&self, path: &str) -> reqwest::Url {
        crate::http::join(&self.api, path)
    }

    async fn version(&self) -> anyhow::Result<Values> {
        let v: ServerVersion = crate::http::get(&self.client, self.url("version")).await?.json().await?;
        Ok(vec![("version".into(), Value::Text(v.version))])
    }

    async fn repositories(&self) -> anyhow::Result<Vec<Repository>> {
        let mut url = self.url("repos/search");
        url.query_pairs_mut()
            .append_pair("limit", &self.repos_max.to_string())
            .append_pair("sort", "updated")
            .append_pair("order", "desc");
        let results: SearchResults = crate::http::get(&self.client, url).await?.json().await?;
        anyhow::ensure!(results.ok, "GET /api/v1/repos/search: ok is false");
        Ok(results.data)
    }

    async fn open_total(&self, kind: &str, key: &str) -> anyhow::Result<Values> {
        let mut url = self.url("repos/issues/search");
        url.query_pairs_mut().append_pair("state", "open").append_pair("type", kind).append_pair("limit", "1");
        let response = crate::http::get(&self.client, url).await?;
        let raw = response
            .headers()
            .get("x-total-count")
            .ok_or_else(|| anyhow::anyhow!("GET /api/v1/repos/issues/search?type={kind}: no X-Total-Count header"))?;
        let total: f64 = raw
            .to_str()
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .ok_or_else(|| anyhow::anyhow!("X-Total-Count {raw:?} is not a number"))?;
        Ok(vec![(key.to_owned(), Value::Number(total))])
    }

    async fn notifications(&self) -> anyhow::Result<Values> {
        let n: NotificationCount = crate::http::get(&self.client, self.url("notifications/new")).await?.json().await?;
        Ok(vec![("notifications".into(), Value::Number(n.new))])
    }

    /// Failed runs of one repository in the last 24 h; `None` when the server answers 404.
    async fn failed_runs(&self, repo: &Repository, now: DateTime<Utc>) -> anyhow::Result<Option<usize>> {
        let mut url = self.url(&format!("repos/{}/actions/tasks", repo.full_name));
        url.query_pairs_mut().append_pair("status", "failure").append_pair("limit", &RUNS_PAGE.to_string());
        let path = url.path().to_owned();
        let response = self.client.get(url).send().await.with_context(|| format!("GET {path}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}");
        let tasks: ActionTaskResponse = response.json().await?;
        let since = now - chrono::Duration::seconds(DAY_S);
        Ok(Some(tasks.workflow_runs.iter().filter(|t| t.status == "failure" && t.created_at > since).count()))
    }

    async fn poll_at(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<(String, Value)>> {
        let (version, repos, issues, prs, notifications) = tokio::join!(
            self.version(),
            self.repositories(),
            self.open_total("issues", "issues_open_total"),
            self.open_total("pulls", "prs_open_total"),
            self.notifications(),
        );
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        for (part, result) in [
            ("version", version),
            ("issues/search issues", issues),
            ("issues/search pulls", prs),
            ("notifications/new", notifications),
        ] {
            match result {
                Ok(values) => partial.ok(values),
                Err(e) => partial.fail(part, &e),
            }
        }
        let mut repos = match repos {
            Ok(repos) => repos,
            Err(e) => {
                partial.fail("repos/search", &e);
                return partial.finish();
            }
        };
        repos.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
        partial.ok(repo_values(&repos, self.top, now));
        if self.actions {
            let recent = &repos[..self.top.min(repos.len())];
            let runs = futures::future::join_all(recent.iter().map(|r| self.failed_runs(r, now))).await;
            let mut total = None;
            for (repo, result) in recent.iter().zip(runs) {
                match result {
                    Ok(Some(failed)) => {
                        *total.get_or_insert(0) += failed;
                        let key = format!("repo/{}/actions_failed_24h", crate::key::sanitize(&repo.name));
                        partial.ok([(key, Value::Number(failed as f64))]);
                    }
                    Ok(None) => partial.ok([]),
                    Err(e) => partial.fail(&format!("actions {}", repo.full_name), &e),
                }
            }
            if let Some(total) = total {
                partial.ok([("actions_failed_24h".into(), Value::Number(total as f64))]);
            }
        }
        partial.finish()
    }
}

/// Totals over every repository, plus per-repository keys for the first `top` (newest first).
fn repo_values(repos: &[Repository], top: usize, now: DateTime<Utc>) -> Values {
    let age = |r: &Repository| (now - r.updated_at).num_seconds().max(0) as f64;
    let sum = |f: fn(&Repository) -> f64| repos.iter().map(f).sum::<f64>();
    let count = |f: fn(&Repository) -> bool| repos.iter().filter(|r| f(r)).count() as f64;
    let mut out: Values = vec![
        ("repos".into(), Value::Number(repos.len() as f64)),
        ("repos_private".into(), Value::Number(count(|r| r.private))),
        ("repos_public".into(), Value::Number(count(|r| !r.private))),
        ("repos_archived".into(), Value::Number(count(|r| r.archived))),
        ("stars".into(), Value::Number(sum(|r| r.stars_count))),
        ("forks".into(), Value::Number(sum(|r| r.forks_count))),
        ("open_issues".into(), Value::Number(sum(|r| r.open_issues_count))),
        ("open_prs".into(), Value::Number(sum(|r| r.open_pr_counter))),
        ("size_kb".into(), Value::Number(sum(|r| r.size))),
    ];
    if let Some(latest) = repos.first() {
        out.push(("latest_repo".into(), Value::Text(latest.name.clone())));
        out.push(("latest_repo_age".into(), Value::Number(age(latest))));
    }
    for repo in repos.iter().take(top) {
        let key = crate::key::sanitize(&repo.name);
        out.push((format!("repo/{key}/stars"), Value::Number(repo.stars_count)));
        out.push((format!("repo/{key}/issues"), Value::Number(repo.open_issues_count)));
        out.push((format!("repo/{key}/prs"), Value::Number(repo.open_pr_counter)));
        out.push((format!("repo/{key}/age"), Value::Number(age(repo))));
    }
    out
}

#[async_trait]
impl Plugin for Forgejo {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        self.poll_at(Utc::now()).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::TimeZone;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TOKEN: &str = "test-token-not-real";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap()
    }

    fn repo(
        id: u64,
        name: &str,
        private: bool,
        archived: bool,
        counts: [u64; 5],
        updated_at: &str,
    ) -> serde_json::Value {
        let [stars, forks, issues, prs, size] = counts;
        json!({
            "id": id,
            "owner": {"id": 7, "login": "owner", "email": "owner@noreply.example.invalid"},
            "name": name,
            "full_name": format!("gio/{name}"),
            "description": "",
            "empty": false,
            "private": private,
            "fork": false,
            "template": false,
            "mirror": false,
            "size": size,
            "language": "Rust",
            "html_url": format!("https://forgejo.example.invalid/gio/{name}"),
            "stars_count": stars,
            "forks_count": forks,
            "watchers_count": 1,
            "open_issues_count": issues,
            "open_pr_counter": prs,
            "release_counter": 0,
            "default_branch": "main",
            "archived": archived,
            "created_at": "2025-01-01T10:00:00+01:00",
            "updated_at": updated_at,
            "has_actions": true
        })
    }

    async fn mount(server: &MockServer, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/{route}")))
            .and(header("authorization", format!("token {TOKEN}").as_str()))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn instance() -> MockServer {
        let server = MockServer::start().await;
        let ok = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
        mount(&server, "version", ok(json!({"version": "13.0.2+gitea-1.22.0"}))).await;
        let repos = json!({"ok": true, "data": [
            repo(3, "peephole", false, false, [4, 1, 2, 1, 1200], "2026-10-25T12:30:00+01:00"),
            repo(2, "homelab", true, false, [0, 0, 5, 2, 300], "2026-10-24T12:00:00Z"),
            repo(1, "old", false, true, [1, 2, 0, 0, 100], "2026-01-01T00:00:00Z"),
        ]});
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/search"))
            .and(query_param("limit", "50"))
            .and(query_param("sort", "updated"))
            .and(query_param("order", "desc"))
            .respond_with(ok(repos))
            .mount(&server)
            .await;
        for (kind, total) in [("issues", "17"), ("pulls", "3")] {
            Mock::given(method("GET"))
                .and(path("/api/v1/repos/issues/search"))
                .and(query_param("state", "open"))
                .and(query_param("type", kind))
                .and(query_param("limit", "1"))
                .respond_with(ok(json!([])).insert_header("X-Total-Count", total))
                .mount(&server)
                .await;
        }
        mount(&server, "notifications/new", ok(json!({"new": 4}))).await;
        server
    }

    fn plugin(server: &MockServer, extra: &str) -> Forgejo {
        let yaml = format!("url: {}\ntoken: {TOKEN}\n{extra}", server.uri());
        Forgejo::new("git", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Forgejo) -> BTreeMap<String, Value> {
        p.poll_at(now()).await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn summarises_the_instance_and_the_recent_repositories() {
        let server = instance().await;
        let got = poll(&plugin(&server, "top: 2")).await;
        let n = |k: &str| got[k].as_f64().unwrap_or_else(|| panic!("{k} is not a number"));
        assert_eq!(got["version"], Value::Text("13.0.2+gitea-1.22.0".into()));
        assert_eq!(n("repos"), 3.0);
        assert_eq!(n("repos_private"), 1.0);
        assert_eq!(n("repos_public"), 2.0);
        assert_eq!(n("repos_archived"), 1.0);
        assert_eq!(n("stars"), 5.0);
        assert_eq!(n("forks"), 3.0);
        assert_eq!(n("open_issues"), 7.0);
        assert_eq!(n("open_prs"), 3.0);
        assert_eq!(n("size_kb"), 1600.0);
        assert_eq!(got["latest_repo"], Value::Text("peephole".into()));
        assert_eq!(n("latest_repo_age"), 1800.0);
        assert_eq!(n("repo/peephole/stars"), 4.0);
        assert_eq!(n("repo/peephole/issues"), 2.0);
        assert_eq!(n("repo/peephole/prs"), 1.0);
        assert_eq!(n("repo/peephole/age"), 1800.0);
        assert_eq!(n("repo/homelab/age"), 86_400.0);
        assert!(!got.keys().any(|k| k.starts_with("repo/old/")));
        assert_eq!(n("issues_open_total"), 17.0);
        assert_eq!(n("prs_open_total"), 3.0);
        assert_eq!(n("notifications"), 4.0);
        assert_eq!(n("_requests_failed"), 0.0);
        assert!(!got.keys().any(|k| k.contains("actions")));
        assert!(!got.values().any(|v| v.as_text().is_some_and(|t| t.contains('@'))));
    }

    #[tokio::test]
    async fn latest_repository_is_the_newest_even_when_the_server_does_not_sort() {
        let server = MockServer::start().await;
        let repos = json!({"ok": true, "data": [
            repo(1, "older", false, false, [0; 5], "2026-10-20T12:00:00Z"),
            repo(2, "newer", false, false, [0; 5], "2026-10-25T11:00:00Z"),
        ]});
        mount(&server, "repos/search", ResponseTemplate::new(200).set_body_json(repos)).await;
        let got = poll(&plugin(&server, "top: 1")).await;
        assert_eq!(got["latest_repo"], Value::Text("newer".into()));
        assert!(got.contains_key("repo/newer/age") && !got.contains_key("repo/older/age"));
    }

    #[tokio::test]
    async fn a_missing_total_header_fails_only_that_part() {
        let server = MockServer::start().await;
        mount(&server, "repos/issues/search", ResponseTemplate::new(200).set_body_json(json!([]))).await;
        mount(&server, "notifications/new", ResponseTemplate::new(200).set_body_json(json!({"new": 0}))).await;
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["notifications"], Value::Number(0.0));
        assert!(!got.contains_key("issues_open_total") && !got.contains_key("prs_open_total"));
    }

    #[tokio::test]
    async fn counts_failed_action_runs_of_the_last_day_and_skips_404() {
        let server = instance().await;
        let runs = json!({"total_count": 3, "workflow_runs": [
            {"id": 9, "name": "ci", "status": "failure", "event": "push", "run_number": 9,
             "created_at": "2026-10-25T10:00:00Z", "updated_at": "2026-10-25T10:05:00Z"},
            {"id": 8, "name": "ci", "status": "failure", "event": "push", "run_number": 8,
             "created_at": "2026-10-24T13:30:00+01:00", "updated_at": "2026-10-24T13:35:00+01:00"},
            {"id": 7, "name": "ci", "status": "failure", "event": "push", "run_number": 7,
             "created_at": "2026-10-24T12:00:00Z", "updated_at": "2026-10-24T12:05:00Z"}
        ]});
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/gio/peephole/actions/tasks"))
            .and(query_param("status", "failure"))
            .respond_with(ResponseTemplate::new(200).set_body_json(runs))
            .mount(&server)
            .await;
        mount(&server, "repos/gio/homelab/actions/tasks", ResponseTemplate::new(404)).await;
        let got = poll(&plugin(&server, "top: 2\nactions: true")).await;
        assert_eq!(got["repo/peephole/actions_failed_24h"], Value::Number(2.0));
        assert!(!got.contains_key("repo/homelab/actions_failed_24h"));
        assert_eq!(got["actions_failed_24h"], Value::Number(2.0));
        assert_eq!(got["_requests_failed"], Value::Number(0.0));
    }

    #[tokio::test]
    async fn a_rejected_token_fails_the_poll_without_echoing_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
        let err = format!("{:#}", plugin(&server, "actions: true").poll_at(now()).await.unwrap_err());
        assert!(err.contains("401") && err.contains("/api/v1/version") && !err.contains(TOKEN), "{err}");
    }

    #[test]
    fn defaults_and_validation() {
        let s: Settings = serde_yaml::from_str("{url: 'https://forgejo.example.invalid', token: t}").unwrap();
        assert!(!s.insecure && !s.actions && s.timeout_s.is_none());
        assert_eq!((s.repos_max, s.top), (50, 5));
        assert_eq!(Forgejo::new("g", s).unwrap().interval(), Duration::from_secs(300));
        let zero: Settings =
            serde_yaml::from_str("{url: 'https://forgejo.example.invalid', token: t, repos_max: 0}").unwrap();
        assert!(Forgejo::new("g", zero).err().unwrap().to_string().contains("repos_max"));
        let typo = serde_yaml::from_str::<Settings>("{url: 'https://f.example.invalid', token: t, topp: 1}");
        assert!(typo.is_err());
    }
}
