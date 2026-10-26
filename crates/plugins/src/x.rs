//! An X account's public counters and its latest posts through X API v2.
//!
//! Endpoints, checked on 2026-10-25 against the server's published OpenAPI
//! (`https://api.x.com/2/openapi.json`, version 2.172),
//! <https://docs.x.com/x-api/users/get-user-by-username> and
//! <https://docs.x.com/x-api/users/get-posts>:
//! - `GET /2/users/by/username/{username}?user.fields=public_metrics,created_at,verified` →
//!   `{"data": {"id", "created_at", "verified", "public_metrics": {"followers_count",
//!   "following_count", "post_count", "listed_count", "like_count"?, "media_count"?}}}`, or
//!   `{"errors": [Problem]}` with no `data` when the account does not exist.
//! - `GET /2/users/{id}/tweets?max_results=5&post.fields=public_metrics,created_at&exclude=retweets,replies`
//!   → `{"data"?: [{"id", "text", "created_at", "public_metrics": {"repost_count",
//!   "reply_count", "like_count", "quote_count", "bookmark_count", "impression_count"}}],
//!   "meta": {"result_count"}}`; `data` is absent when there are no posts.
//! - Failures answer `application/problem+json` (`type`, `title`, `detail`, `status`) or
//!   `{"code", "message"}`.
//!
//! The older names `tweet_count` and `retweet_count` are accepted too. Auth is
//! `Authorization: Bearer <app bearer token>`; user lookup needs at least the Basic access
//! tier (the Free tier answers 403 "Client Forbidden").

use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the struct holds the bearer token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub bearer_token: String,
    pub username: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Also read the latest posts.
    #[serde(default = "default_true")]
    pub posts: bool,
    #[serde(default = "default_api_url")]
    pub api_url: String,
}

fn default_interval() -> u64 {
    900
}
fn default_true() -> bool {
    true
}
fn default_api_url() -> String {
    "https://api.x.com".into()
}

pub struct X {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    api: reqwest::Url,
    username: String,
    posts: bool,
    user_id: Mutex<Option<String>>,
}

impl X {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = crate::period::timeout(settings.timeout_s, interval)?;
        anyhow::ensure!(!settings.username.trim().is_empty(), "username must not be empty");
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &Default::default(), Some(&settings.bearer_token), false)?,
            api: crate::http::base_url(&settings.api_url)?,
            username: settings.username.trim_start_matches('@').to_owned(),
            posts: settings.posts,
            user_id: Mutex::new(None),
        })
    }

    fn url(&self, path: &str) -> reqwest::Url {
        crate::http::join(&self.api, path)
    }

    async fn fetch<T: for<'de> Deserialize<'de>>(&self, url: reqwest::Url) -> anyhow::Result<Envelope<T>> {
        let path = url.path().to_owned();
        let response = self.client.get(url).send().await.with_context(|| format!("GET {path}"))?;
        let status = response.status();
        let text = response.text().await.with_context(|| format!("GET {path}"))?;
        anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}{}", describe(&text));
        let envelope: Envelope<T> = serde_json::from_str(&text).with_context(|| format!("GET {path}"))?;
        if envelope.data.is_none()
            && let Some(first) = envelope.errors.first()
        {
            anyhow::bail!("GET {path}: {}", first.summary());
        }
        Ok(envelope)
    }

    async fn user(&self, now: DateTime<Utc>) -> anyhow::Result<Values> {
        let mut url = self.url(&format!("2/users/by/username/{}", self.username));
        url.query_pairs_mut().append_pair("user.fields", "public_metrics,created_at,verified");
        let path = url.path().to_owned();
        let user: UserData =
            self.fetch(url).await?.data.ok_or_else(|| anyhow::anyhow!("GET {path}: response has no data"))?;
        *self.user_id.lock().expect("user id poisoned") = Some(user.id.clone());
        let m = &user.public_metrics;
        Ok(vec![
            ("followers".into(), Value::Number(m.followers_count)),
            ("following".into(), Value::Number(m.following_count)),
            ("posts".into(), Value::Number(m.post_count)),
            ("listed".into(), Value::Number(m.listed_count)),
            ("verified".into(), Value::Number(if user.verified { 1.0 } else { 0.0 })),
            ("account_age_days".into(), Value::Number((now - user.created_at).num_days().max(0) as f64)),
        ])
    }

    async fn latest_posts(&self, id: &str, now: DateTime<Utc>) -> anyhow::Result<Values> {
        let mut url = self.url(&format!("2/users/{id}/tweets"));
        url.query_pairs_mut()
            .append_pair("max_results", "5")
            .append_pair("post.fields", "public_metrics,created_at")
            .append_pair("exclude", "retweets,replies");
        let posts: Vec<Post> = self.fetch(url).await?.data.unwrap_or_default();
        Ok(post_values(&posts, now))
    }

    async fn poll_at(&self, now: DateTime<Utc>) -> anyhow::Result<Vec<(String, Value)>> {
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        match self.user(now).await {
            Ok(values) => partial.ok(values),
            Err(e) => partial.fail("users/by/username", &e),
        }
        let id = self.user_id.lock().expect("user id poisoned").clone();
        if self.posts
            && let Some(id) = id
        {
            match self.latest_posts(&id, now).await {
                Ok(values) => partial.ok(values),
                Err(e) => partial.fail("users/tweets", &e),
            }
        }
        partial.finish()
    }
}

const TEXT_CHARS: usize = 80;

type Values = Vec<(String, Value)>;

#[derive(Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<Problem>,
}

/// An RFC 7807 problem (`title`, `detail`) or the older `{code, message}` error.
#[derive(Deserialize)]
struct Problem {
    title: Option<String>,
    detail: Option<String>,
    message: Option<String>,
}

impl Problem {
    fn summary(&self) -> String {
        let parts: Vec<&str> =
            [&self.title, &self.detail, &self.message].into_iter().flatten().map(String::as_str).collect();
        if parts.is_empty() { "no error message".into() } else { parts.join(": ") }
    }
}

/// `": <title>: <detail>"` out of an error body, or nothing when it is not a known shape.
fn describe(body: &str) -> String {
    #[derive(Deserialize)]
    struct Body {
        #[serde(flatten)]
        problem: Problem,
        #[serde(default)]
        errors: Vec<Problem>,
    }
    let Ok(parsed) = serde_json::from_str::<Body>(body) else { return String::new() };
    let problem = match (&parsed.problem, parsed.errors.first()) {
        (p, _) if p.title.is_some() || p.detail.is_some() || p.message.is_some() => p,
        (_, Some(first)) => first,
        _ => return String::new(),
    };
    format!(": {}", problem.summary())
}

#[derive(Deserialize)]
struct UserData {
    id: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    verified: bool,
    public_metrics: UserMetrics,
}

#[derive(Deserialize)]
struct UserMetrics {
    followers_count: f64,
    following_count: f64,
    #[serde(alias = "tweet_count")]
    post_count: f64,
    listed_count: f64,
}

#[derive(Deserialize)]
struct Post {
    text: String,
    created_at: DateTime<Utc>,
    public_metrics: PostMetrics,
}

#[derive(Deserialize)]
struct PostMetrics {
    #[serde(alias = "retweet_count")]
    repost_count: f64,
    reply_count: f64,
    like_count: f64,
    impression_count: f64,
}

fn post_values(posts: &[Post], now: DateTime<Utc>) -> Values {
    let Some(newest) = posts.iter().max_by_key(|p| p.created_at) else { return Vec::new() };
    let m = &newest.public_metrics;
    let text: String = newest.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(TEXT_CHARS).collect();
    vec![
        ("last_post_age".into(), Value::Number((now - newest.created_at).num_seconds().max(0) as f64)),
        ("last_post_likes".into(), Value::Number(m.like_count)),
        ("last_post_reposts".into(), Value::Number(m.repost_count)),
        ("last_post_replies".into(), Value::Number(m.reply_count)),
        ("last_post_views".into(), Value::Number(m.impression_count)),
        ("last_post_text".into(), Value::Text(text)),
        ("recent_likes".into(), Value::Number(posts.iter().map(|p| p.public_metrics.like_count).sum())),
        ("recent_views".into(), Value::Number(posts.iter().map(|p| p.public_metrics.impression_count).sum())),
    ]
}

#[async_trait]
impl Plugin for X {
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

    const TOKEN: &str = "AAAAtest-bearer-not-real";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 12, 0, 0).unwrap()
    }

    fn user_body() -> serde_json::Value {
        json!({"data": {
            "id": "2244994945",
            "name": "Octo",
            "username": "octo",
            "created_at": "2025-10-24T11:00:00.000Z",
            "verified": true,
            "public_metrics": {
                "followers_count": 1500, "following_count": 80, "post_count": 940,
                "listed_count": 12, "like_count": 300, "media_count": 40
            }
        }})
    }

    fn post(id: &str, text: &str, created_at: &str, metrics: [u64; 4]) -> serde_json::Value {
        let [likes, reposts, replies, views] = metrics;
        json!({
            "id": id,
            "text": text,
            "created_at": created_at,
            "edit_history_post_ids": [id],
            "public_metrics": {
                "repost_count": reposts, "reply_count": replies, "like_count": likes,
                "quote_count": 0, "bookmark_count": 1, "impression_count": views
            }
        })
    }

    async fn mount_user(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/2/users/by/username/octo"))
            .and(query_param("user.fields", "public_metrics,created_at,verified"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn mount_posts(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/2/users/2244994945/tweets"))
            .and(query_param("max_results", "5"))
            .and(query_param("post.fields", "public_metrics,created_at"))
            .and(query_param("exclude", "retweets,replies"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn long_text() -> String {
        format!("Peephole v0.2 is out\n\nthree new plugins: forgejo, github and x. {} end", "café ".repeat(20))
    }

    fn posts_body() -> serde_json::Value {
        json!({
            "data": [
                post("3", "older one", "2026-10-24T12:00:00.000Z", [10, 1, 0, 400]),
                post("4", &long_text(), "2026-10-25T11:00:00.000Z", [25, 3, 2, 1200]),
            ],
            "meta": {"result_count": 2, "newest_id": "4", "oldest_id": "3", "next_token": "7140dibdnow9c7btw4"}
        })
    }

    fn plugin(server: &MockServer, extra: &str) -> X {
        let yaml = format!("bearer_token: {TOKEN}\nusername: octo\napi_url: {}\n{extra}", server.uri());
        X::new("x", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &X) -> BTreeMap<String, Value> {
        p.poll_at(now()).await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn reads_the_account_and_its_latest_posts() {
        let server = MockServer::start().await;
        mount_user(&server, ResponseTemplate::new(200).set_body_json(user_body())).await;
        mount_posts(&server, ResponseTemplate::new(200).set_body_json(posts_body())).await;
        let got = poll(&plugin(&server, "")).await;
        let n = |k: &str| got[k].as_f64().unwrap_or_else(|| panic!("{k} is not a number"));
        assert_eq!(n("followers"), 1500.0);
        assert_eq!(n("following"), 80.0);
        assert_eq!(n("posts"), 940.0);
        assert_eq!(n("listed"), 12.0);
        assert_eq!(n("verified"), 1.0);
        assert_eq!(n("account_age_days"), 366.0);
        assert_eq!(n("last_post_age"), 3600.0);
        assert_eq!(n("last_post_likes"), 25.0);
        assert_eq!(n("last_post_reposts"), 3.0);
        assert_eq!(n("last_post_replies"), 2.0);
        assert_eq!(n("last_post_views"), 1200.0);
        assert_eq!(n("recent_likes"), 35.0);
        assert_eq!(n("recent_views"), 1600.0);
        let text = got["last_post_text"].as_text().unwrap();
        assert_eq!(text.chars().count(), 80);
        assert!(text.starts_with("Peephole v0.2 is out three new plugins") && !text.contains('\n'), "{text}");
        assert_eq!(n("_requests_failed"), 0.0);
    }

    #[tokio::test]
    async fn no_posts_is_not_an_error() {
        let server = MockServer::start().await;
        mount_user(&server, ResponseTemplate::new(200).set_body_json(user_body())).await;
        mount_posts(&server, ResponseTemplate::new(200).set_body_json(json!({"meta": {"result_count": 0}}))).await;
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["followers"], Value::Number(1500.0));
        assert!(!got.keys().any(|k| k.starts_with("last_post") || k.starts_with("recent_")));
        assert_eq!(got["_requests_failed"], Value::Number(0.0));
    }

    #[tokio::test]
    async fn posts_can_be_turned_off() {
        let server = MockServer::start().await;
        mount_user(&server, ResponseTemplate::new(200).set_body_json(user_body())).await;
        Mock::given(path("/2/users/2244994945/tweets"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let got = poll(&plugin(&server, "posts: false")).await;
        assert_eq!(got["posts"], Value::Number(940.0));
        server.verify().await;
    }

    #[tokio::test]
    async fn the_user_id_survives_a_failed_lookup() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2/users/by/username/octo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(user_body()))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2/users/by/username/octo"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        mount_posts(&server, ResponseTemplate::new(200).set_body_json(posts_body())).await;
        let p = plugin(&server, "");
        poll(&p).await;
        let got = poll(&p).await;
        assert!(!got.contains_key("followers"));
        assert_eq!(got["last_post_likes"], Value::Number(25.0));
        assert_eq!(got["_requests_failed"], Value::Number(1.0));
    }

    #[tokio::test]
    async fn problems_become_errors_with_title_and_detail_but_no_token() {
        let server = MockServer::start().await;
        let forbidden = json!({
            "client_id": "12345",
            "detail": "When authenticating requests to the Twitter API v2 endpoints, you must use keys and tokens from a Twitter developer App that is attached to a Project.",
            "registration_url": "https://developer.twitter.com/en/docs/projects/overview",
            "title": "Client Forbidden",
            "required_enrollment": "Appropriate Level of API Access",
            "reason": "client-not-enrolled",
            "type": "https://api.twitter.com/2/problems/client-forbidden"
        });
        mount_user(&server, ResponseTemplate::new(403).set_body_raw(forbidden.to_string(), "application/problem+json"))
            .await;
        let err = format!("{:#}", plugin(&server, "").poll_at(now()).await.unwrap_err());
        assert!(
            err.contains("403") && err.contains("Client Forbidden") && err.contains("attached to a Project"),
            "{err}"
        );
        assert!(!err.contains(TOKEN), "{err}");

        let unauthorized = MockServer::start().await;
        let body = json!({"title": "Unauthorized", "type": "about:blank", "status": 401, "detail": "Unauthorized"});
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_json(body))
            .mount(&unauthorized)
            .await;
        let err = format!("{:#}", plugin(&unauthorized, "").poll_at(now()).await.unwrap_err());
        assert!(err.contains("401") && err.contains("Unauthorized") && !err.contains(TOKEN), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_account_is_an_error_naming_it() {
        let server = MockServer::start().await;
        let body = json!({"errors": [{
            "value": "octo", "detail": "Could not find user with username: [octo].", "title": "Not Found Error",
            "resource_type": "user", "parameter": "username", "resource_id": "octo",
            "type": "https://api.twitter.com/2/problems/resource-not-found"
        }]});
        mount_user(&server, ResponseTemplate::new(200).set_body_json(body)).await;
        let err = format!("{:#}", plugin(&server, "").poll_at(now()).await.unwrap_err());
        assert!(err.contains("Not Found Error") && err.contains("Could not find user"), "{err}");
    }

    #[test]
    fn accepts_the_older_metric_names() {
        let user: UserData = serde_json::from_value(json!({
            "id": "1", "created_at": "2020-01-01T00:00:00.000Z", "verified": false,
            "public_metrics": {"followers_count": 1, "following_count": 2, "tweet_count": 3, "listed_count": 4}
        }))
        .unwrap();
        assert_eq!(user.public_metrics.post_count, 3.0);
        let p: Post = serde_json::from_value(json!({
            "id": "1", "text": "t", "created_at": "2020-01-01T00:00:00.000Z",
            "public_metrics": {"retweet_count": 5, "reply_count": 0, "like_count": 0, "quote_count": 0, "impression_count": 0}
        }))
        .unwrap();
        assert_eq!(p.public_metrics.repost_count, 5.0);
    }

    #[test]
    fn defaults_and_validation() {
        let s: Settings = serde_yaml::from_str("{bearer_token: t, username: '@octo'}").unwrap();
        assert!(s.posts && s.timeout_s.is_none());
        assert_eq!(s.api_url, "https://api.x.com");
        let x = X::new("x", s).unwrap();
        assert_eq!(x.interval(), Duration::from_secs(900));
        assert_eq!(x.username, "octo");
        assert!(X::new("x", serde_yaml::from_str("{bearer_token: t, username: ''}").unwrap()).is_err());
        assert!(serde_yaml::from_str::<Settings>("{bearer_token: t, username: o, post: false}").is_err());
    }
}
