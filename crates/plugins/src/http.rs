use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

/// Upper bound for one HTTP request when a plugin has no `timeout_s` setting.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Header values may be credentials: every configured header is marked sensitive
/// so it never shows up in debug output.
pub(crate) fn client(
    timeout: Duration,
    headers: &BTreeMap<String, String>,
    bearer: Option<&str>,
    insecure: bool,
) -> anyhow::Result<reqwest::Client> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).with_context(|| format!("invalid header name {name:?}"))?;
        map.insert(name.clone(), sensitive(value).with_context(|| format!("invalid value for header {name}"))?);
    }
    if let Some(token) = bearer {
        map.insert(AUTHORIZATION, sensitive(&format!("Bearer {token}")).context("invalid bearer_token")?);
    }
    reqwest::Client::builder()
        .timeout(timeout)
        .default_headers(map)
        .danger_accept_invalid_certs(insecure)
        .build()
        .context("building the HTTP client")
}

fn sensitive(value: &str) -> anyhow::Result<HeaderValue> {
    let mut v = HeaderValue::from_str(value)?;
    v.set_sensitive(true);
    Ok(v)
}

/// A request error without its URL (query strings may carry credentials).
pub(crate) fn scrub(error: reqwest::Error) -> anyhow::Error {
    anyhow::Error::from(error.without_url())
}

/// GET `url` and fail on any non-2xx status.
pub(crate) async fn get(client: &reqwest::Client, url: reqwest::Url) -> anyhow::Result<reqwest::Response> {
    let path = url.path().to_owned();
    let response = client.get(url).send().await.map_err(scrub).with_context(|| format!("GET {path}"))?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}");
    Ok(response)
}

/// Join `path` onto a configured base URL, keeping any path the base already has.
pub(crate) fn join(base: &reqwest::Url, path: &str) -> reqwest::Url {
    let mut url = base.clone();
    let joined = format!("{}/{}", base.path().trim_end_matches('/'), path.trim_start_matches('/'));
    url.set_path(&joined);
    url
}

/// Parse a configured base URL, failing fast on anything but http(s).
pub(crate) fn base_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).with_context(|| format!("invalid url {raw:?}"))?;
    anyhow::ensure!(matches!(url.scheme(), "http" | "https"), "url {raw:?} must be http or https");
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_keeps_the_base_path() {
        let base = base_url("http://mimir.example.invalid/prometheus/").unwrap();
        assert_eq!(join(&base, "/api/v1/query").as_str(), "http://mimir.example.invalid/prometheus/api/v1/query");
        let bare = base_url("http://prom.example.invalid").unwrap();
        assert_eq!(join(&bare, "api/v1/query").as_str(), "http://prom.example.invalid/api/v1/query");
    }

    #[tokio::test]
    async fn request_errors_never_carry_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = client(Duration::from_secs(2), &BTreeMap::new(), None, false).unwrap();
        let url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/api/stats?token=supersecret")).unwrap();
        let err = format!("{:#}", get(&client, url).await.unwrap_err());
        assert!(err.contains("GET /api/stats"), "{err}");
        assert!(!err.contains("supersecret"), "{err}");
    }

    #[test]
    fn rejects_non_http_urls_and_bad_headers() {
        assert!(base_url("ftp://example.invalid").is_err());
        assert!(base_url("not a url").is_err());
        let bad = BTreeMap::from([("bad header".to_owned(), "x".to_owned())]);
        assert!(client(DEFAULT_TIMEOUT, &bad, None, false).is_err());
        let newline = BTreeMap::from([("x-ok".to_owned(), "a\nb".to_owned())]);
        assert!(client(DEFAULT_TIMEOUT, &newline, None, false).is_err());
    }
}
