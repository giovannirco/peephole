//! A GitHub account's public profile, owned repositories and recent public activity.
//!
//! Endpoints, checked on 2026-10-25 against <https://docs.github.com/en/rest> (users, repos,
//! activity/events, rate limits, pagination, event types) and live `api.github.com` responses:
//! - `GET /users/{user}` → `followers`, `following`, `public_repos`, `public_gists`.
//! - `GET /users/{user}/repos?per_page=100&type=owner&sort=pushed`, following the
//!   `Link: <…>; rel="next"` header (absolute URLs, `/user/{id}/repos?…&page=N`) for at most
//!   three pages, and only on the configured API origin.
//! - `GET /users/{user}/events/public?per_page=100`: the array is not sorted by `created_at`.
//!   `PushEvent` payloads carry only `repository_id`, `push_id`, `ref`, `head` and `before`
//!   (no commit count), so the plugin counts pushes, not commits.
//! - `X-RateLimit-Limit` / `X-RateLimit-Remaining` on every response; an exhausted limit
//!   answers 403 or 429 with `X-RateLimit-Remaining: 0`.
//!
//! Requests send `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`,
//! `User-Agent: peephole` and, with a token, `Authorization: Bearer <token>`.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the struct may hold a token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Account login.
    pub user: String,
    #[serde(default)]
    pub token: Option<String>,
    /// Unauthenticated callers get 60 requests per hour; one poll makes three to five.
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Repository names that get `repo/<name>/…` keys.
    #[serde(default)]
    pub repos: Vec<String>,
    #[serde(default = "default_api_url")]
    pub api_url: String,
}

fn default_interval() -> u64 {
    600
}
fn default_api_url() -> String {
    "https://api.github.com".into()
}

pub struct Github {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    api: reqwest::Url,
    user: String,
    repos: Vec<String>,
}

impl Github {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = crate::period::timeout(settings.timeout_s, interval)?;
        anyhow::ensure!(!settings.user.trim().is_empty(), "user must not be empty");
        let headers = [
            ("Accept".to_owned(), "application/vnd.github+json".to_owned()),
            ("X-GitHub-Api-Version".to_owned(), "2022-11-28".to_owned()),
            ("User-Agent".to_owned(), "peephole".to_owned()),
        ]
        .into();
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &headers, settings.token.as_deref(), false)?,
            api: crate::http::base_url(&settings.api_url)?,
            user: settings.user,
            repos: settings.repos,
        })
    }

    fn url(&self, path: &str) -> reqwest::Url {
        crate::http::join(&self.api, path)
    }

    async fn fetch<T: for<'de> Deserialize<'de>>(&self, url: reqwest::Url) -> anyhow::Result<Page<T>> {
        let path = url.path().to_owned();
        let response =
            self.client.get(url).send().await.map_err(crate::http::scrub).with_context(|| format!("GET {path}"))?;
        let status = response.status();
        let rate = Rate::from_headers(response.headers());
        let limited = matches!(status.as_u16(), 403 | 429) && rate.is_some_and(|r| r.remaining == 0.0);
        if limited {
            let limit = rate.map_or(0.0, |r| r.limit);
            anyhow::bail!("GET {path}: HTTP {status}: rate limit exhausted (limit {limit} requests per hour)");
        }
        anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}");
        let next = next_link(response.headers());
        Ok(Page { body: response.json().await.with_context(|| format!("GET {path}"))?, rate, next })
    }

    async fn profile(&self) -> anyhow::Result<(Values, Option<Rate>)> {
        let page: Page<User> = self.fetch(self.url(&format!("users/{}", self.user))).await?;
        let u = page.body;
        let values = vec![
            ("followers".into(), Value::Number(u.followers)),
            ("following".into(), Value::Number(u.following)),
            ("public_repos".into(), Value::Number(u.public_repos)),
            ("public_gists".into(), Value::Number(u.public_gists)),
        ];
        Ok((values, page.rate))
    }

    async fn repositories(&self) -> anyhow::Result<(Vec<Repository>, Option<Rate>)> {
        let mut url = self.url(&format!("users/{}/repos", self.user));
        url.query_pairs_mut().append_pair("per_page", "100").append_pair("type", "owner").append_pair("sort", "pushed");
        let mut repos = Vec::new();
        let mut rate = None;
        for _ in 0..MAX_PAGES {
            let page: Page<Vec<Repository>> = self.fetch(url).await?;
            repos.extend(page.body);
            rate = Rate::lowest(rate, page.rate);
            let Some(next) = page.next else { break };
            let next = reqwest::Url::parse(&next).with_context(|| "Link rel=next is not a URL")?;
            anyhow::ensure!(
                next.origin() == self.api.origin(),
                "Link rel=next points away from the configured api_url; not following it"
            );
            url = next;
        }
        Ok((repos, rate))
    }

    async fn events(&self, now: DateTime<Utc>) -> anyhow::Result<(Values, Option<Rate>)> {
        let mut url = self.url(&format!("users/{}/events/public", self.user));
        url.query_pairs_mut().append_pair("per_page", "100");
        let page: Page<Vec<Event>> = self.fetch(url).await?;
        Ok((event_values(&page.body, now), page.rate))
    }

    async fn poll_at(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<(String, Value)>> {
        let (profile, repos, events) = tokio::join!(self.profile(), self.repositories(), self.events(now));
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        let mut rate = None;
        for (part, result) in [("user", profile), ("events", events)] {
            match result {
                Ok((values, r)) => {
                    rate = Rate::lowest(rate, r);
                    partial.ok(values);
                }
                Err(e) => partial.fail(part, &e),
            }
        }
        match repos {
            Ok((repos, r)) => {
                rate = Rate::lowest(rate, r);
                partial.ok(repo_values(&repos, now));
                for wanted in &self.repos {
                    match repos.iter().find(|r| r.name.eq_ignore_ascii_case(wanted)) {
                        Some(repo) => partial.ok(one_repo(wanted, repo, now)),
                        None => partial.fail(
                            &format!("repo {wanted}"),
                            &anyhow::anyhow!("not among the account's first {} owned repositories", repos.len()),
                        ),
                    }
                }
            }
            Err(e) => partial.fail("repos", &e),
        }
        if let Some(r) = rate {
            partial.ok([
                ("rate_remaining".into(), Value::Number(r.remaining)),
                ("rate_limit".into(), Value::Number(r.limit)),
            ]);
        }
        partial.finish()
    }
}

const MAX_PAGES: usize = 3;
const DAY_S: i64 = 86_400;

type Values = Vec<(String, Value)>;

struct Page<T> {
    body: T,
    rate: Option<Rate>,
    next: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rate {
    limit: f64,
    remaining: f64,
}

impl Rate {
    fn from_headers(headers: &reqwest::header::HeaderMap) -> Option<Self> {
        let number = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<f64>().ok();
        Some(Self { limit: number("x-ratelimit-limit")?, remaining: number("x-ratelimit-remaining")? })
    }

    /// The reading with the fewest requests left: the most recent one within a poll.
    fn lowest(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(a), Some(b)) => Some(if b.remaining < a.remaining { b } else { a }),
            (a, b) => a.or(b),
        }
    }
}

/// The `rel="next"` target of an RFC 8288 `Link` header.
fn next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let link = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (target, params) = part.trim().split_once(';')?;
        let is_next = params.split(';').any(|p| p.trim() == "rel=\"next\"");
        is_next.then(|| target.trim().trim_start_matches('<').trim_end_matches('>').to_owned())
    })
}

#[derive(Deserialize)]
struct User {
    followers: f64,
    following: f64,
    public_repos: f64,
    public_gists: f64,
}

#[derive(Deserialize)]
struct Repository {
    name: String,
    stargazers_count: u64,
    forks_count: u64,
    open_issues_count: u64,
    language: Option<String>,
    pushed_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    created_at: DateTime<Utc>,
}

fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    (now - at).num_seconds().max(0) as f64
}

fn repo_values(repos: &[Repository], now: DateTime<Utc>) -> Values {
    let sum = |f: fn(&Repository) -> u64| repos.iter().map(f).sum::<u64>() as f64;
    let mut languages = std::collections::BTreeMap::<&str, usize>::new();
    for language in repos.iter().filter_map(|r| r.language.as_deref()) {
        *languages.entry(language).or_default() += 1;
    }
    let mut out: Values = vec![
        ("repos_total".into(), Value::Number(repos.len() as f64)),
        ("stars".into(), Value::Number(sum(|r| r.stargazers_count))),
        ("forks".into(), Value::Number(sum(|r| r.forks_count))),
        ("open_issues".into(), Value::Number(sum(|r| r.open_issues_count))),
        ("languages".into(), Value::Number(languages.len() as f64)),
    ];
    let top_language = languages.iter().max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)));
    if let Some((language, _)) = top_language {
        out.push(("top_language".into(), Value::Text((*language).to_owned())));
    }
    if let Some((repo, at)) = repos.iter().filter_map(|r| Some((r, r.pushed_at?))).max_by_key(|(_, at)| *at) {
        out.push(("last_push_repo".into(), Value::Text(repo.name.clone())));
        out.push(("last_push_age".into(), Value::Number(age(at, now))));
    }
    let top = repos.iter().max_by(|a, b| a.stargazers_count.cmp(&b.stargazers_count).then(b.name.cmp(&a.name)));
    if let Some(repo) = top {
        out.push(("top_repo".into(), Value::Text(repo.name.clone())));
        out.push(("top_repo_stars".into(), Value::Number(repo.stargazers_count as f64)));
    }
    out
}

fn one_repo(wanted: &str, repo: &Repository, now: DateTime<Utc>) -> Values {
    let key = crate::key::sanitize(wanted);
    let mut out = vec![
        (format!("repo/{key}/stars"), Value::Number(repo.stargazers_count as f64)),
        (format!("repo/{key}/forks"), Value::Number(repo.forks_count as f64)),
        (format!("repo/{key}/issues"), Value::Number(repo.open_issues_count as f64)),
    ];
    if let Some(at) = repo.pushed_at {
        out.push((format!("repo/{key}/push_age"), Value::Number(age(at, now))));
    }
    out
}

fn event_values(events: &[Event], now: DateTime<Utc>) -> Values {
    let since = now - chrono::Duration::seconds(DAY_S);
    let recent: Vec<&Event> = events.iter().filter(|e| e.created_at > since).collect();
    let pushes = recent.iter().filter(|e| e.kind == "PushEvent").count();
    let mut out: Values = vec![
        ("events_24h".into(), Value::Number(recent.len() as f64)),
        ("pushes_24h".into(), Value::Number(pushes as f64)),
    ];
    if let Some(newest) = events.iter().max_by_key(|e| e.created_at) {
        out.push(("last_event".into(), Value::Text(newest.kind.clone())));
        out.push(("last_event_age".into(), Value::Number(age(newest.created_at, now))));
    }
    out
}

#[async_trait]
impl Plugin for Github {
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
    use wiremock::matchers::{header, header_exists, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    const TOKEN: &str = "ghp_test-token-not-real";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap()
    }

    fn rated(status: u16, remaining: u32) -> ResponseTemplate {
        limited(status, 60, remaining)
    }

    fn limited(status: u16, limit: u32, remaining: u32) -> ResponseTemplate {
        ResponseTemplate::new(status)
            .insert_header("x-ratelimit-limit", limit.to_string().as_str())
            .insert_header("x-ratelimit-remaining", remaining.to_string().as_str())
            .insert_header("x-ratelimit-used", (limit - remaining).to_string().as_str())
            .insert_header("x-ratelimit-resource", "core")
            .insert_header("x-ratelimit-reset", "1792981590")
    }

    fn repo(
        name: &str,
        stars: u64,
        forks: u64,
        issues: u64,
        language: Option<&str>,
        pushed: Option<&str>,
    ) -> serde_json::Value {
        json!({
            "id": stars + 1000,
            "name": name,
            "full_name": format!("octo/{name}"),
            "private": false,
            "owner": {"login": "octo", "id": 170270, "type": "User"},
            "html_url": format!("https://github.example.invalid/octo/{name}"),
            "fork": false,
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2026-10-25T00:00:00Z",
            "pushed_at": pushed,
            "size": 120,
            "stargazers_count": stars,
            "watchers_count": stars,
            "language": language,
            "forks_count": forks,
            "archived": false,
            "open_issues_count": issues,
            "default_branch": "main"
        })
    }

    fn page_link(server: &MockServer, page: u32) -> String {
        format!(
            "<{}/user/170270/repos?per_page=100&type=owner&sort=pushed&page={page}>; rel=\"next\", <{}/user/170270/repos?per_page=100&type=owner&sort=pushed&page=9>; rel=\"last\"",
            server.uri(),
            server.uri()
        )
    }

    async fn user(server: &MockServer) {
        let body = json!({
            "login": "octo", "id": 170270, "type": "User", "name": "Octo",
            "public_repos": 3, "public_gists": 2, "followers": 120, "following": 7,
            "created_at": "2014-01-01T00:00:00Z", "updated_at": "2026-10-01T00:00:00Z"
        });
        Mock::given(method("GET"))
            .and(path("/users/octo"))
            .respond_with(rated(200, 59).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn repos_pages(server: &MockServer) {
        let first = json!([
            repo("peephole", 40, 3, 2, Some("Rust"), Some("2026-10-25T11:00:00Z")),
            repo("dotfiles", 5, 1, 0, Some("Shell"), Some("2026-10-20T12:00:00Z")),
        ]);
        Mock::given(method("GET"))
            .and(path("/users/octo/repos"))
            .and(query_param("per_page", "100"))
            .and(query_param("type", "owner"))
            .and(query_param("sort", "pushed"))
            .respond_with(rated(200, 58).set_body_json(first).insert_header("link", page_link(server, 2).as_str()))
            .mount(server)
            .await;
        let second = json!([
            repo("Tools", 9, 0, 4, Some("Rust"), Some("2026-10-25T11:30:00Z")),
            repo("empty", 0, 0, 0, None, None),
        ]);
        Mock::given(method("GET"))
            .and(path("/user/170270/repos"))
            .and(query_param("page", "2"))
            .respond_with(rated(200, 57).set_body_json(second))
            .mount(server)
            .await;
    }

    async fn events(server: &MockServer) {
        let push = |at: &str| {
            json!({"id": "1", "type": "PushEvent", "actor": {"login": "octo"}, "repo": {"name": "octo/peephole"},
                   "payload": {"repository_id": 1, "push_id": 2, "ref": "refs/heads/main", "head": "a", "before": "b"},
                   "public": true, "created_at": at})
        };
        let body = json!([
            push("2026-10-25T05:44:23Z"),
            push("2026-10-25T00:44:51Z"),
            {"id": "3", "type": "WatchEvent", "actor": {"login": "octo"}, "repo": {"name": "other/x"},
             "payload": {"action": "started"}, "public": true, "created_at": "2026-10-25T06:15:10Z"},
            push("2026-10-23T22:25:07Z"),
        ]);
        Mock::given(method("GET"))
            .and(path("/users/octo/events/public"))
            .and(query_param("per_page", "100"))
            .respond_with(rated(200, 56).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn account() -> MockServer {
        let server = MockServer::start().await;
        user(&server).await;
        repos_pages(&server).await;
        events(&server).await;
        server
    }

    fn plugin(server: &MockServer, extra: &str) -> Github {
        let yaml = format!("user: octo\napi_url: {}\n{extra}", server.uri());
        Github::new("gh", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Github) -> BTreeMap<String, Value> {
        p.poll_at(now()).await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn summarises_profile_repositories_and_activity() {
        let server = account().await;
        let got = poll(&plugin(&server, "")).await;
        let n = |k: &str| got[k].as_f64().unwrap_or_else(|| panic!("{k} is not a number"));
        assert_eq!(n("followers"), 120.0);
        assert_eq!(n("following"), 7.0);
        assert_eq!(n("public_repos"), 3.0);
        assert_eq!(n("public_gists"), 2.0);
        assert_eq!(n("repos_total"), 4.0);
        assert_eq!(n("stars"), 54.0);
        assert_eq!(n("forks"), 4.0);
        assert_eq!(n("open_issues"), 6.0);
        assert_eq!(n("languages"), 2.0);
        assert_eq!(got["top_language"], Value::Text("Rust".into()));
        assert_eq!(got["last_push_repo"], Value::Text("Tools".into()));
        assert_eq!(n("last_push_age"), 1800.0);
        assert_eq!(got["top_repo"], Value::Text("peephole".into()));
        assert_eq!(n("top_repo_stars"), 40.0);
        assert_eq!(n("events_24h"), 3.0);
        assert_eq!(n("pushes_24h"), 2.0);
        assert_eq!(got["last_event"], Value::Text("WatchEvent".into()));
        assert_eq!(n("last_event_age"), 20_690.0);
        assert_eq!(n("rate_limit"), 60.0);
        assert_eq!(n("rate_remaining"), 56.0);
        assert_eq!(n("_requests_failed"), 0.0);
        assert!(!got.keys().any(|k| k.starts_with("repo/")));
    }

    #[tokio::test]
    async fn configured_repositories_get_their_own_keys() {
        let server = account().await;
        let got = poll(&plugin(&server, "repos: [peephole, tools, missing]")).await;
        assert_eq!(got["repo/peephole/stars"], Value::Number(40.0));
        assert_eq!(got["repo/peephole/forks"], Value::Number(3.0));
        assert_eq!(got["repo/peephole/issues"], Value::Number(2.0));
        assert_eq!(got["repo/peephole/push_age"], Value::Number(3600.0));
        assert_eq!(got["repo/tools/stars"], Value::Number(9.0));
        assert!(!got.keys().any(|k| k.starts_with("repo/missing/")));
        assert_eq!(got["_requests_failed"], Value::Number(1.0));
    }

    #[tokio::test]
    async fn pagination_stops_after_three_pages() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/octo/repos"))
            .and(query_param_is_missing("page"))
            .respond_with(
                rated(200, 59)
                    .set_body_json(json!([repo("p1", 1, 0, 0, None, None)]))
                    .insert_header("link", page_link(&server, 2).as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;
        for page in 2..=4u32 {
            let body = json!([repo(&format!("p{page}"), 1, 0, 0, None, None)]);
            Mock::given(method("GET"))
                .and(path("/user/170270/repos"))
                .and(query_param("page", page.to_string().as_str()))
                .respond_with(
                    rated(200, 59).set_body_json(body).insert_header("link", page_link(&server, page + 1).as_str()),
                )
                .expect(if page == 4 { 0 } else { 1 })
                .mount(&server)
                .await;
        }
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["repos_total"], Value::Number(3.0));
        server.verify().await;
    }

    #[tokio::test]
    async fn a_next_link_to_another_origin_is_not_followed() {
        let server = MockServer::start().await;
        user(&server).await;
        let elsewhere = "<https://evil.example.invalid/user/1/repos?page=2>; rel=\"next\"";
        Mock::given(method("GET"))
            .and(path("/users/octo/repos"))
            .respond_with(
                rated(200, 59).set_body_json(json!([repo("p1", 1, 0, 0, None, None)])).insert_header("link", elsewhere),
            )
            .mount(&server)
            .await;
        let got = poll(&plugin(&server, "")).await;
        assert!(!got.contains_key("repos_total"));
        assert_eq!(got["followers"], Value::Number(120.0));
        assert_eq!(got["_requests_failed"], Value::Number(2.0));
    }

    #[tokio::test]
    async fn an_exhausted_rate_limit_is_an_error_naming_the_limit_not_the_token() {
        let server = MockServer::start().await;
        let body = json!({"message": "API rate limit exceeded for 203.0.113.9.", "documentation_url": "https://docs.github.com/rest"});
        Mock::given(method("GET")).respond_with(rated(403, 0).set_body_json(body)).mount(&server).await;
        let p = plugin(&server, &format!("token: {TOKEN}"));
        let err = format!("{:#}", p.poll_at(now()).await.unwrap_err());
        assert!(err.contains("rate limit") && err.contains("60"), "{err}");
        assert!(!err.contains(TOKEN) && !err.contains("203.0.113.9"), "{err}");
    }

    #[tokio::test]
    async fn sends_the_token_as_bearer_only_when_configured() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/octo"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .and(header("user-agent", "peephole"))
            .and(header("x-github-api-version", "2022-11-28"))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(
                limited(200, 5000, 4999)
                    .set_body_json(json!({"followers": 1, "following": 0, "public_repos": 0, "public_gists": 0})),
            )
            .mount(&server)
            .await;
        let got = poll(&plugin(&server, &format!("token: {TOKEN}"))).await;
        assert_eq!(got["followers"], Value::Number(1.0));
        assert_eq!(got["rate_limit"], Value::Number(5000.0));

        let anonymous = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&anonymous)
            .await;
        Mock::given(method("GET"))
            .and(path("/users/octo"))
            .respond_with(
                rated(200, 59)
                    .set_body_json(json!({"followers": 2, "following": 0, "public_repos": 0, "public_gists": 0})),
            )
            .mount(&anonymous)
            .await;
        let got = poll(&plugin(&anonymous, "")).await;
        assert_eq!(got["followers"], Value::Number(2.0));
        let sent: Vec<Request> = anonymous.received_requests().await.unwrap();
        assert!(sent.iter().all(|r| !r.headers.contains_key("authorization")));
    }

    #[test]
    fn defaults_and_validation() {
        let s: Settings = serde_yaml::from_str("{user: octo}").unwrap();
        assert!(s.token.is_none() && s.repos.is_empty() && s.timeout_s.is_none());
        assert_eq!(s.api_url, "https://api.github.com");
        assert_eq!(Github::new("g", s).unwrap().interval(), Duration::from_secs(600));
        assert!(Github::new("g", serde_yaml::from_str("{user: ' '}").unwrap()).is_err());
        assert!(serde_yaml::from_str::<Settings>("{user: octo, repo: [a]}").is_err());
    }
}
