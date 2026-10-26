//! Technitium DNS Server dashboard stats.
//!
//! Endpoint: `GET /api/dashboard/stats/get?token=<token>&type=<range>`, checked against
//! <https://github.com/TechnitiumSoftware/DnsServer/blob/master/APIDOCS.md> ("Get Stats" and
//! "API Response Format"): the body is `{"status": "ok", "response": {"stats": {...},
//! "mainChartData": {"datasets": [{"label", "data"}]}, "topClients": [{"name", "domain", "hits",
//! "rateLimited"}], "topDomains": [{"name", "hits"}], "topBlockedDomains": [{"name", "hits"}]}}`;
//! any other `status` (`error`, `invalid-token`, `2fa-required`) is a failure, with `errorMessage`
//! present for `error`. APIDOCS names `Authorization: Bearer <token>` as the primary form since
//! v15.0 and keeps the `token` query parameter for backward compatibility; this plugin uses the
//! query parameter, which every version accepts, so no error or log carries the URL.

use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the struct holds the API token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Server base URL, e.g. `http://dns.example.invalid:5380`.
    pub url: String,
    /// An API token (`createToken`) with Dashboard: View; never logged.
    pub token: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub range: Range,
    /// How many entries of each top list to emit as `top/<list>/<i>`.
    #[serde(default = "default_top")]
    pub top: usize,
}

fn default_interval() -> u64 {
    60
}
fn default_top() -> usize {
    5
}

/// The stats window, spelled as the API's `type` parameter.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum Range {
    #[default]
    LastHour,
    LastDay,
    LastWeek,
    LastMonth,
    LastYear,
}

impl Range {
    fn as_str(self) -> &'static str {
        match self {
            Range::LastHour => "LastHour",
            Range::LastDay => "LastDay",
            Range::LastWeek => "LastWeek",
            Range::LastMonth => "LastMonth",
            Range::LastYear => "LastYear",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    status: String,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    response: Option<Response>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
    stats: Stats,
    #[serde(default)]
    main_chart_data: Option<Chart>,
    #[serde(default)]
    top_clients: Vec<Top>,
    #[serde(default)]
    top_domains: Vec<Top>,
    #[serde(default)]
    top_blocked_domains: Vec<Top>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stats {
    total_queries: f64,
    total_server_failure: f64,
    total_nx_domain: f64,
    total_refused: f64,
    total_authoritative: f64,
    total_recursive: f64,
    total_cached: f64,
    total_blocked: f64,
    total_dropped: f64,
    total_clients: f64,
    zones: f64,
    cached_entries: f64,
    allowed_zones: f64,
    blocked_zones: f64,
    allow_list_zones: f64,
    block_list_zones: f64,
}

#[derive(Deserialize)]
struct Chart {
    #[serde(default)]
    datasets: Vec<Dataset>,
}

#[derive(Deserialize)]
struct Dataset {
    label: String,
    #[serde(default)]
    data: Vec<f64>,
}

/// An entry of `topClients` (`name` is the client address), `topDomains` or `topBlockedDomains`.
#[derive(Deserialize)]
struct Top {
    name: String,
    hits: f64,
}

type Values = Vec<(String, Value)>;

pub struct Technitium {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    url: reqwest::Url,
    top: usize,
}

impl Technitium {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(t) => crate::period::seconds(t, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        let mut url = crate::http::join(&crate::http::base_url(&settings.url)?, "api/dashboard/stats/get");
        url.query_pairs_mut().append_pair("token", &settings.token).append_pair("type", settings.range.as_str());
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &Default::default(), None, settings.insecure)?,
            url,
            top: settings.top,
        })
    }

    /// Fetch the stats; every error names the path only, never the URL with its token.
    async fn fetch(&self) -> anyhow::Result<Response> {
        let path = self.url.path().to_owned();
        let scrub = |e: reqwest::Error| anyhow::Error::new(e.without_url()).context(format!("GET {path}"));
        let response = self.client.get(self.url.clone()).send().await.map_err(scrub)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(scrub)?;
        let body: Envelope = serde_json::from_slice(&bytes)
            .map_err(|e| anyhow::anyhow!("GET {path}: HTTP {status}: response is not a Technitium API body ({e})"))?;
        if body.status != "ok" {
            anyhow::bail!(
                "GET {path}: {}: {}",
                body.status,
                body.error_message.as_deref().unwrap_or("no error message")
            );
        }
        anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}");
        body.response.ok_or_else(|| anyhow::anyhow!("GET {path}: status ok but no response"))
    }
}

fn ratio(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { part / whole } else { 0.0 }
}

fn stats_values(s: &Stats) -> Values {
    [
        ("queries", s.total_queries),
        ("blocked", s.total_blocked),
        ("blocked_ratio", ratio(s.total_blocked, s.total_queries)),
        ("clients", s.total_clients),
        ("cached", s.total_cached),
        ("cached_ratio", ratio(s.total_cached, s.total_queries)),
        ("nxdomain", s.total_nx_domain),
        ("servfail", s.total_server_failure),
        ("refused", s.total_refused),
        ("dropped", s.total_dropped),
        ("recursive", s.total_recursive),
        ("authoritative", s.total_authoritative),
        ("zones", s.zones),
        ("cached_entries", s.cached_entries),
        ("blocked_zones", s.blocked_zones),
        ("allowed_zones", s.allowed_zones),
        ("blocklist_zones", s.block_list_zones),
        ("allowlist_zones", s.allow_list_zones),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), Value::Number(v)))
    .collect()
}

fn series_values(chart: Option<&Chart>) -> Values {
    let Some(chart) = chart else { return Vec::new() };
    [("queries_series", "Total"), ("blocked_series", "Blocked"), ("clients_series", "Clients")]
        .into_iter()
        .filter_map(|(key, label)| {
            let set = chart.datasets.iter().find(|d| d.label == label)?;
            Some((key.to_owned(), Value::Series(set.data.clone())))
        })
        .collect()
}

fn top_values(r: &Response, top: usize) -> Values {
    let mut out = Vec::new();
    for (key, list) in
        [("top_domain", &r.top_domains), ("top_blocked", &r.top_blocked_domains), ("top_client", &r.top_clients)]
    {
        if let Some(first) = list.first() {
            out.push((key.to_owned(), Value::Text(first.name.clone())));
            out.push((format!("{key}_hits"), Value::Number(first.hits)));
        }
    }
    for (list_key, list) in [("domain", &r.top_domains), ("blocked", &r.top_blocked_domains)] {
        for (i, entry) in list.iter().take(top).enumerate() {
            out.push((format!("top/{list_key}/{i}"), Value::Text(entry.name.clone())));
            out.push((format!("top/{list_key}/{i}/hits"), Value::Number(entry.hits)));
        }
    }
    out
}

#[async_trait]
impl Plugin for Technitium {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let r = self.fetch().await?;
        let mut values = stats_values(&r.stats);
        values.extend(series_values(r.main_chart_data.as_ref()));
        values.extend(top_values(&r, self.top));
        Ok(values)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TOKEN: &str = "tok-0123456789abcdef-not-real";

    fn dataset(label: &str, data: &[f64]) -> serde_json::Value {
        json!({"label": label, "backgroundColor": "rgba(102, 153, 255, 0.1)", "borderColor": "rgb(102, 153, 255)",
               "borderWidth": 2, "fill": true, "data": data})
    }

    fn stats_body() -> serde_json::Value {
        json!({
            "response": {
                "stats": {
                    "totalQueries": 925, "totalNoError": 834, "totalServerFailure": 1, "totalNxDomain": 90,
                    "totalRefused": 2, "totalAuthoritative": 47, "totalRecursive": 348, "totalCached": 481,
                    "totalBlocked": 49, "totalDropped": 3, "totalClients": 6, "zones": 19, "cachedEntries": 6330,
                    "allowedZones": 10, "blockedZones": 1, "allowListZones": 0, "blockListZones": 307447
                },
                "mainChartData": {
                    "labelFormat": "HH:mm",
                    "labels": ["2024-02-04T10:38:00.0000000Z", "2024-02-04T10:39:00.0000000Z", "2024-02-04T10:40:00.0000000Z"],
                    "datasets": [
                        dataset("Total", &[4.0, 6.0, 13.0]),
                        dataset("No Error", &[4.0, 6.0, 11.0]),
                        dataset("Blocked", &[0.0, 1.0, 2.0]),
                        dataset("Clients", &[2.0, 3.0, 3.0])
                    ]
                },
                "queryResponseChartData": {"labels": ["Authoritative"], "datasets": [{"data": [47]}]},
                "queryTypeChartData": {"labels": ["A", "AAAA"], "datasets": [{"data": [683, 42]}]},
                "protocolTypeChartData": {"labels": ["Udp"], "datasets": [{"data": [925]}]},
                "topClients": [
                    {"name": "192.168.10.5", "domain": "server1.home", "hits": 463, "rateLimited": false},
                    {"name": "192.168.10.13", "hits": 165, "rateLimited": false}
                ],
                "topDomains": [
                    {"name": "www.example.invalid", "hits": 114},
                    {"name": "api.example.invalid", "hits": 61},
                    {"name": "cdn.example.invalid", "hits": 52}
                ],
                "topBlockedDomains": [
                    {"name": "ads.example.invalid", "hits": 10}
                ]
            },
            "status": "ok"
        })
    }

    async fn server(range: &str, response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/dashboard/stats/get"))
            .and(query_param("token", TOKEN))
            .and(query_param("type", range))
            .respond_with(response)
            .mount(&server)
            .await;
        server
    }

    fn plugin_at(uri: &str, extra: &str) -> Technitium {
        let yaml = format!("url: {uri}\ntoken: {TOKEN}\n{extra}");
        Technitium::new("dns", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reads_totals_ratios_series_and_top_lists() {
        let server = server("LastHour", ResponseTemplate::new(200).set_body_json(stats_body())).await;
        let got: BTreeMap<_, _> = plugin_at(&server.uri(), "top: 2").poll().await.unwrap().into_iter().collect();
        let n = |k: &str| got[k].as_f64().unwrap_or_else(|| panic!("{k} is not a number"));
        let t = |k: &str| got[k].as_text().unwrap_or_else(|| panic!("{k} is not text")).to_owned();
        assert_eq!(n("queries"), 925.0);
        assert_eq!(n("blocked"), 49.0);
        assert_eq!(n("blocked_ratio"), 49.0 / 925.0);
        assert_eq!(n("clients"), 6.0);
        assert_eq!(n("cached"), 481.0);
        assert_eq!(n("cached_ratio"), 481.0 / 925.0);
        assert_eq!(n("nxdomain"), 90.0);
        assert_eq!(n("servfail"), 1.0);
        assert_eq!(n("refused"), 2.0);
        assert_eq!(n("dropped"), 3.0);
        assert_eq!(n("recursive"), 348.0);
        assert_eq!(n("authoritative"), 47.0);
        assert_eq!(n("zones"), 19.0);
        assert_eq!(n("cached_entries"), 6330.0);
        assert_eq!(n("blocked_zones"), 1.0);
        assert_eq!(n("allowed_zones"), 10.0);
        assert_eq!(n("blocklist_zones"), 307447.0);
        assert_eq!(n("allowlist_zones"), 0.0);
        assert_eq!(got["queries_series"], Value::Series(vec![4.0, 6.0, 13.0]));
        assert_eq!(got["blocked_series"], Value::Series(vec![0.0, 1.0, 2.0]));
        assert_eq!(got["clients_series"], Value::Series(vec![2.0, 3.0, 3.0]));
        assert_eq!(t("top_domain"), "www.example.invalid");
        assert_eq!(n("top_domain_hits"), 114.0);
        assert_eq!(t("top_blocked"), "ads.example.invalid");
        assert_eq!(n("top_blocked_hits"), 10.0);
        assert_eq!(t("top_client"), "192.168.10.5");
        assert_eq!(n("top_client_hits"), 463.0);
        assert_eq!(t("top/domain/0"), "www.example.invalid");
        assert_eq!(n("top/domain/0/hits"), 114.0);
        assert_eq!(t("top/domain/1"), "api.example.invalid");
        assert_eq!(n("top/domain/1/hits"), 61.0);
        assert!(!got.contains_key("top/domain/2"));
        assert_eq!(t("top/blocked/0"), "ads.example.invalid");
        assert_eq!(n("top/blocked/0/hits"), 10.0);
        assert!(!got.contains_key("top/blocked/1"));
        assert!(!got.values().any(|v| v.as_text().is_some_and(|s| s.contains(TOKEN))));
    }

    #[tokio::test]
    async fn sends_the_configured_range_and_tolerates_empty_lists_and_no_queries() {
        let mut body = stats_body();
        body["response"]["stats"]["totalQueries"] = json!(0);
        body["response"]["mainChartData"]["datasets"] = json!([]);
        for list in ["topClients", "topDomains", "topBlockedDomains"] {
            body["response"][list] = json!([]);
        }
        let server = server("LastWeek", ResponseTemplate::new(200).set_body_json(body)).await;
        let got: BTreeMap<_, _> =
            plugin_at(&server.uri(), "range: LastWeek").poll().await.unwrap().into_iter().collect();
        assert_eq!(got["blocked_ratio"], Value::Number(0.0));
        assert_eq!(got["cached_ratio"], Value::Number(0.0));
        assert!(!got.keys().any(|k| k.starts_with("top") || k.ends_with("_series")), "{:?}", got.keys());
    }

    #[tokio::test]
    async fn a_status_other_than_ok_is_an_error_quoting_the_message() {
        let body = json!({"status": "error", "errorMessage": "Access was denied.", "stackTrace": "at Foo()"});
        let server = server("LastHour", ResponseTemplate::new(200).set_body_json(body)).await;
        let err = format!("{:#}", plugin_at(&server.uri(), "").poll().await.unwrap_err());
        assert!(err.contains("Access was denied.") && !err.contains(TOKEN), "{err}");
        let server = server_with(json!({"status": "invalid-token"})).await;
        let err = format!("{:#}", plugin_at(&server.uri(), "").poll().await.unwrap_err());
        assert!(err.contains("invalid-token") && !err.contains(TOKEN), "{err}");
    }

    async fn server_with(body: serde_json::Value) -> MockServer {
        server("LastHour", ResponseTemplate::new(200).set_body_json(body)).await
    }

    #[tokio::test]
    async fn no_failure_ever_prints_the_token() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let uri = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let err = format!("{:#}", plugin_at(&uri, "").poll().await.unwrap_err());
        assert!(err.contains("GET /api/dashboard/stats/get"), "{err}");
        assert!(!err.contains(TOKEN) && !err.contains("token="), "{err}");
        let broken = server("LastHour", ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>")).await;
        let err = format!("{:#}", plugin_at(&broken.uri(), "").poll().await.unwrap_err());
        assert!(err.contains("502") && !err.contains(TOKEN), "{err}");
        let garbage = server("LastHour", ResponseTemplate::new(200).set_body_string("not json")).await;
        let err = format!("{:#}", plugin_at(&garbage.uri(), "").poll().await.unwrap_err());
        assert!(!err.contains(TOKEN), "{err}");
    }

    #[test]
    fn settings_defaults_and_validation() {
        let s: Settings = serde_yaml::from_str("{url: 'http://dns.example.invalid:5380', token: t}").unwrap();
        assert_eq!((s.interval_s, s.range, s.top, s.timeout_s, s.insecure), (60, Range::LastHour, 5, None, false));
        assert_eq!(Technitium::new("d", s).unwrap().interval(), Duration::from_secs(60));
        let err = serde_yaml::from_str::<Settings>("{url: 'http://d.example.invalid', token: t, range: Custom}")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("Custom") && err.contains("LastHour"), "{err}");
        for bad in ["interval_s: 0", "timeout_s: 0"] {
            let yaml = format!("{{url: 'http://d.example.invalid', token: t, {bad}}}");
            let err = Technitium::new("d", serde_yaml::from_str(&yaml).unwrap()).err().unwrap().to_string();
            assert!(err.contains(bad.split(':').next().unwrap()), "{err}");
        }
        let err = Technitium::new("d", serde_yaml::from_str("{url: 'ftp://d.example.invalid', token: t}").unwrap())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("http"), "{err}");
    }
}
