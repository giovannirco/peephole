//! Uptime Kuma through its Prometheus endpoint, `GET /metrics`.
//!
//! Kuma answers 404 unless the Prometheus API is enabled in its settings, and
//! guards the route with HTTP Basic auth. With API keys enabled (the case this
//! plugin supports) the password is the key and the username is ignored.
//! Gauges read: `monitor_status` (0 DOWN, 1 UP, 2 PENDING, 3 MAINTENANCE),
//! `monitor_response_time` (ms, -1 when the last heartbeat had no ping) and
//! `monitor_cert_days_remaining`, labelled `monitor_name` and `monitor_type`.
//! The `monitor_url` and `monitor_hostname` labels are never read: they can
//! hold internal names or URLs with credentials. Monitors are keyed by name;
//! `down_list` is sorted by name.
//!
//! Verified against louislam/uptime-kuma at 413857f: `server/prometheus.js`
//! (gauge and label names, -1 response time), `server/server.js` (the
//! `/metrics` route and its `prometheusApiEnabled` gate), `server/auth.js`
//! (`apiAuth`: express-basic-auth with the key as password) and `src/util.ts`
//! (status constants).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the struct holds the API key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Uptime Kuma base URL; `/metrics` is appended.
    pub url: String,
    /// Sent as the Basic auth password; never logged.
    pub api_key: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Accept a self-signed certificate.
    #[serde(default)]
    pub insecure: bool,
    /// How many names `down_list` holds.
    #[serde(default = "default_list_max")]
    pub list_max: usize,
}

fn default_interval() -> u64 {
    60
}
fn default_list_max() -> usize {
    5
}

const USERNAME: &str = "peephole";

pub struct UptimeKuma {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    metrics: reqwest::Url,
    api_key: String,
    list_max: usize,
}

impl UptimeKuma {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = match settings.timeout_s {
            Some(s) => crate::period::seconds(s, "timeout_s")?,
            None => crate::http::DEFAULT_TIMEOUT.min(interval),
        };
        let base = crate::http::base_url(&settings.url)?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &BTreeMap::new(), None, settings.insecure)?,
            metrics: crate::http::join(&base, "metrics"),
            api_key: settings.api_key,
            list_max: settings.list_max,
        })
    }
}

#[async_trait]
impl Plugin for UptimeKuma {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let path = self.metrics.path().to_owned();
        let response = self
            .client
            .get(self.metrics.clone())
            .basic_auth(USERNAME, Some(&self.api_key))
            .send()
            .await
            .map_err(crate::http::scrub)
            .with_context(|| format!("GET {path}"))?;
        let status = response.status();
        anyhow::ensure!(
            status != reqwest::StatusCode::NOT_FOUND,
            "GET {path}: HTTP {status} (is the Prometheus API enabled in Uptime Kuma's settings?)"
        );
        anyhow::ensure!(status.is_success(), "GET {path}: HTTP {status}");
        let text = response.text().await.with_context(|| format!("GET {path}: reading the body"))?;
        Ok(summarise(parse(&text)?, self.list_max))
    }
}

const DOWN: f64 = 0.0;
const UP: f64 = 1.0;
const PENDING: f64 = 2.0;
const MAINTENANCE: f64 = 3.0;

/// One monitor out of the `monitor_*` gauges, keyed by its `monitor_name` label.
#[derive(Default)]
struct Monitor {
    kind: Option<String>,
    state: Option<f64>,
    response_ms: Option<f64>,
    cert_days: Option<f64>,
}

type Values = Vec<(String, Value)>;

fn monitors(samples: Vec<Sample>) -> BTreeMap<String, Monitor> {
    let mut monitors: BTreeMap<String, Monitor> = BTreeMap::new();
    for mut sample in samples {
        let field = match sample.name.as_str() {
            "monitor_status" => |m: &mut Monitor, v| m.state = Some(v),
            "monitor_response_time" => |m: &mut Monitor, v| m.response_ms = Some(v),
            "monitor_cert_days_remaining" => |m: &mut Monitor, v| m.cert_days = Some(v),
            _ => continue,
        };
        let Some(name) = sample.labels.remove("monitor_name") else { continue };
        let monitor = monitors.entry(name).or_default();
        if monitor.kind.is_none() {
            monitor.kind = sample.labels.remove("monitor_type");
        }
        field(monitor, sample.value);
    }
    monitors.retain(|_, m| m.state.is_some());
    monitors
}

fn summarise(samples: Vec<Sample>, list_max: usize) -> Values {
    let monitors = monitors(samples);
    let mut out = Values::new();
    let count = |state: f64| monitors.values().filter(|m| m.state == Some(state)).count() as f64;
    let (up, down, pending, maintenance) = (count(UP), count(DOWN), count(PENDING), count(MAINTENANCE));
    let total = monitors.len() as f64;
    let counted = total - maintenance;
    for (key, value) in [
        ("monitors_total", total),
        ("monitors_up", up),
        ("monitors_down", down),
        ("monitors_pending", pending),
        ("monitors_maintenance", maintenance),
        ("up_ratio", if counted > 0.0 { up / counted } else { 0.0 }),
    ] {
        out.push((key.into(), Value::Number(value)));
    }
    let mut slowest: Option<(&str, f64)> = None;
    let mut soonest: Option<(&str, f64)> = None;
    let mut known = Vec::new();
    for (name, monitor) in &monitors {
        let key = format!("monitor/{}", crate::key::sanitize(name));
        let state = monitor.state.unwrap_or(DOWN);
        out.push((format!("{key}/up"), Value::Number(if state == UP { 1.0 } else { 0.0 })));
        out.push((format!("{key}/state"), Value::Number(state)));
        if let Some(kind) = &monitor.kind {
            out.push((format!("{key}/type"), Value::Text(kind.clone())));
        }
        if let Some(ms) = monitor.response_ms.filter(|ms| *ms >= 0.0) {
            out.push((format!("{key}/response_ms"), Value::Number(ms)));
            known.push(ms);
            if slowest.is_none_or(|(_, worst)| ms > worst) {
                slowest = Some((name, ms));
            }
        }
        if let Some(days) = monitor.cert_days {
            out.push((format!("{key}/cert_days"), Value::Number(days)));
            if soonest.is_none_or(|(_, least)| days < least) {
                soonest = Some((name, days));
            }
        }
    }
    let down_names: Vec<&str> =
        monitors.iter().filter(|(_, m)| m.state == Some(DOWN)).map(|(n, _)| n.as_str()).take(list_max).collect();
    out.push(("down_list".into(), Value::Text(down_names.join(", "))));
    if let Some((name, days)) = soonest {
        out.push(("cert_min_days".into(), Value::Number(days)));
        out.push(("cert_soonest".into(), Value::Text(name.to_owned())));
    }
    if let Some((name, ms)) = slowest {
        out.push(("slowest".into(), Value::Text(name.to_owned())));
        out.push(("slowest_ms".into(), Value::Number(ms)));
        out.push(("avg_response_ms".into(), Value::Number(known.iter().sum::<f64>() / known.len() as f64)));
    }
    out
}

#[derive(Debug, PartialEq)]
struct Sample {
    name: String,
    labels: BTreeMap<String, String>,
    value: f64,
}

/// Prometheus text exposition: `name{label="value",...} value [timestamp]` per line.
/// Errors name the line number only: label values can hold URLs with credentials.
fn parse(text: &str) -> anyhow::Result<Vec<Sample>> {
    let mut samples = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let sample =
            parse_line(line).ok_or_else(|| anyhow::anyhow!("metrics line {}: not a Prometheus sample", index + 1))?;
        samples.push(sample);
    }
    Ok(samples)
}

fn parse_line(line: &str) -> Option<Sample> {
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace()).unwrap_or(line.len());
    let name = &line[..name_end];
    if name.is_empty() {
        return None;
    }
    let mut labels = BTreeMap::new();
    let rest = match line[name_end..].strip_prefix('{') {
        Some(inside) => parse_labels(inside, &mut labels)?,
        None => &line[name_end..],
    };
    let mut fields = rest.split_whitespace();
    let value = fields.next()?.parse().ok()?;
    if let Some(timestamp) = fields.next() {
        timestamp.parse::<i64>().ok()?;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(Sample { name: name.to_owned(), labels, value })
}

/// Reads `key="value",...}` and returns what follows the closing brace.
fn parse_labels<'a>(mut rest: &'a str, labels: &mut BTreeMap<String, String>) -> Option<&'a str> {
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix('}') {
            return Some(after);
        }
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        rest = rest[eq + 1..].trim_start().strip_prefix('"')?;
        let mut value = String::new();
        let mut chars = rest.char_indices();
        let end = loop {
            match chars.next()? {
                (i, '"') => break i,
                (_, '\\') => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    escaped => value.push(escaped),
                },
                (_, c) => value.push(c),
            }
        };
        labels.insert(key.to_owned(), value);
        rest = rest[end + 1..].trim_start();
        match rest.strip_prefix(',') {
            Some(after) => rest = after,
            None if rest.starts_with('}') => {}
            None => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{basic_auth, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const KEY: &str = "uk1_test-key-not-real";

    const EXPOSITION: &str = r#"# HELP monitor_cert_days_remaining The number of days remaining until the certificate expires
# TYPE monitor_cert_days_remaining gauge
monitor_cert_days_remaining{env="prod",monitor_id="1",monitor_name="Home Page",monitor_type="http",monitor_url="https://user:hunter2@site.example.invalid/",monitor_hostname="null",monitor_port="null"} 42
# HELP monitor_cert_is_valid Is the certificate still valid? (1 = Yes, 0= No)
# TYPE monitor_cert_is_valid gauge
monitor_cert_is_valid{env="prod",monitor_id="1",monitor_name="Home Page",monitor_type="http",monitor_url="https://user:hunter2@site.example.invalid/",monitor_hostname="null",monitor_port="null"} 1
# HELP monitor_uptime_ratio Uptime ratio calculated over sliding window specified by the 'window' label. (0.0 - 1.0)
# TYPE monitor_uptime_ratio gauge
monitor_uptime_ratio{env="prod",monitor_id="1",monitor_name="Home Page",monitor_type="http",monitor_url="https://user:hunter2@site.example.invalid/",monitor_hostname="null",monitor_port="null",window="1d"} 0.998
# HELP monitor_response_time_seconds Average response time in seconds calculated over sliding window specified by the 'window' label
# TYPE monitor_response_time_seconds gauge
monitor_response_time_seconds{env="prod",monitor_id="1",monitor_name="Home Page",monitor_type="http",monitor_url="https://user:hunter2@site.example.invalid/",monitor_hostname="null",monitor_port="null",window="1d"} 0.131
# HELP monitor_response_time Monitor Response Time (ms)
# TYPE monitor_response_time gauge
monitor_response_time{env="prod",monitor_id="1",monitor_name="Home Page",monitor_type="http",monitor_url="https://user:hunter2@site.example.invalid/",monitor_hostname="null",monitor_port="null"} 120
monitor_response_time{env="",monitor_id="2",monitor_name="API",monitor_type="keyword",monitor_url="https://api.example.invalid/health",monitor_hostname="null",monitor_port="null"} 340
monitor_response_time{env="",monitor_id="3",monitor_name="DB",monitor_type="port",monitor_url="https://",monitor_hostname="db.lan.example.invalid",monitor_port="5432"} -1
monitor_response_time{env="",monitor_id="4",monitor_name="NAS",monitor_type="ping",monitor_url="https://",monitor_hostname="nas.lan.example.invalid",monitor_port="null"} 15
monitor_response_time{env="",monitor_id="5",monitor_name="Mail",monitor_type="port",monitor_url="https://",monitor_hostname="mail.lan.example.invalid",monitor_port="25"} 80
monitor_response_time{env="",monitor_id="6",monitor_name="Backup",monitor_type="push",monitor_url="https://",monitor_hostname="null",monitor_port="null"} 5
# HELP monitor_status Monitor Status (1 = UP, 0= DOWN, 2= PENDING, 3= MAINTENANCE)
# TYPE monitor_status gauge
monitor_status{env="prod",monitor_id="1",monitor_name="Home Page",monitor_type="http",monitor_url="https://user:hunter2@site.example.invalid/",monitor_hostname="null",monitor_port="null"} 1
monitor_status{env="",monitor_id="2",monitor_name="API",monitor_type="keyword",monitor_url="https://api.example.invalid/health",monitor_hostname="null",monitor_port="null"} 1
monitor_status{env="",monitor_id="3",monitor_name="DB",monitor_type="port",monitor_url="https://",monitor_hostname="db.lan.example.invalid",monitor_port="5432"} 0
monitor_status{env="",monitor_id="4",monitor_name="NAS",monitor_type="ping",monitor_url="https://",monitor_hostname="nas.lan.example.invalid",monitor_port="null"} 0
monitor_status{env="",monitor_id="5",monitor_name="Mail",monitor_type="port",monitor_url="https://",monitor_hostname="mail.lan.example.invalid",monitor_port="25"} 2
monitor_status{env="",monitor_id="6",monitor_name="Backup",monitor_type="push",monitor_url="https://",monitor_hostname="null",monitor_port="null"} 3
# HELP process_cpu_user_seconds_total Total user CPU time spent in seconds.
# TYPE process_cpu_user_seconds_total counter
process_cpu_user_seconds_total 1.23
"#;

    async fn kuma(body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/kuma/metrics"))
            .and(basic_auth("peephole", KEY))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(401)).with_priority(10).mount(&server).await;
        server
    }

    fn plugin(server: &MockServer, extra: &str) -> UptimeKuma {
        let yaml = format!("url: {}/kuma\napi_key: {KEY}\n{extra}", server.uri());
        UptimeKuma::new("kuma", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &UptimeKuma) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn counts_states_and_reports_each_monitor() {
        let server = kuma(EXPOSITION).await;
        let got = poll(&plugin(&server, "")).await;
        let n = |k: &str| got[k].as_f64().unwrap_or_else(|| panic!("{k} is not a number"));
        let t = |k: &str| got[k].as_text().unwrap_or_else(|| panic!("{k} is not text")).to_owned();
        assert_eq!(n("monitors_total"), 6.0);
        assert_eq!(n("monitors_up"), 2.0);
        assert_eq!(n("monitors_down"), 2.0);
        assert_eq!(n("monitors_pending"), 1.0);
        assert_eq!(n("monitors_maintenance"), 1.0);
        assert_eq!(n("up_ratio"), 0.4);
        assert_eq!(n("monitor/Home-Page/up"), 1.0);
        assert_eq!(n("monitor/Home-Page/state"), 1.0);
        assert_eq!(n("monitor/Home-Page/response_ms"), 120.0);
        assert_eq!(n("monitor/Home-Page/cert_days"), 42.0);
        assert_eq!(t("monitor/Home-Page/type"), "http");
        assert_eq!(n("monitor/DB/up"), 0.0);
        assert_eq!(n("monitor/DB/state"), 0.0);
        assert!(!got.contains_key("monitor/DB/response_ms"));
        assert!(!got.contains_key("monitor/DB/cert_days"));
        assert_eq!(n("monitor/Mail/up"), 0.0);
        assert_eq!(n("monitor/Mail/state"), 2.0);
        assert_eq!(n("monitor/Backup/state"), 3.0);
        assert_eq!(t("monitor/Backup/type"), "push");
        assert_eq!(t("down_list"), "DB, NAS");
        assert_eq!(n("cert_min_days"), 42.0);
        assert_eq!(t("cert_soonest"), "Home Page");
        assert_eq!(t("slowest"), "API");
        assert_eq!(n("slowest_ms"), 340.0);
        assert_eq!(n("avg_response_ms"), 112.0);
    }

    #[tokio::test]
    async fn never_emits_urls_or_hostnames() {
        let server = kuma(EXPOSITION).await;
        let got = poll(&plugin(&server, "")).await;
        for (key, value) in &got {
            let text = format!("{key} {value:?}");
            for secret in ["hunter2", "site.example.invalid", "api.example.invalid", "lan.example.invalid", "5432"] {
                assert!(!text.contains(secret), "{text} leaks {secret}");
            }
        }
    }

    #[tokio::test]
    async fn down_list_is_capped_and_empty_when_all_is_well() {
        let server = kuma(EXPOSITION).await;
        assert_eq!(poll(&plugin(&server, "list_max: 1")).await["down_list"], Value::Text("DB".into()));
        let healthy = kuma("monitor_status{monitor_name=\"A\",monitor_type=\"http\"} 1\n").await;
        let got = poll(&plugin(&healthy, "")).await;
        assert_eq!(got["down_list"], Value::Text(String::new()));
        assert_eq!(got["up_ratio"], Value::Number(1.0));
        assert!(!got.contains_key("cert_min_days") && !got.contains_key("slowest"));
    }

    #[tokio::test]
    async fn no_monitors_and_all_in_maintenance_give_a_zero_ratio() {
        let empty = kuma("process_cpu_user_seconds_total 1\n").await;
        let got = poll(&plugin(&empty, "")).await;
        assert_eq!(got["monitors_total"], Value::Number(0.0));
        assert_eq!(got["up_ratio"], Value::Number(0.0));
        let maintenance = kuma("monitor_status{monitor_name=\"A\",monitor_type=\"http\"} 3\n").await;
        assert_eq!(poll(&plugin(&maintenance, "")).await["up_ratio"], Value::Number(0.0));
    }

    #[tokio::test]
    async fn a_wrong_key_fails_without_echoing_it() {
        let server = kuma(EXPOSITION).await;
        let yaml = format!("url: {}/kuma\napi_key: uk9_wrong-key-value", server.uri());
        let p = UptimeKuma::new("kuma", serde_yaml::from_str(&yaml).unwrap()).unwrap();
        let err = format!("{:#}", p.poll().await.unwrap_err());
        assert!(err.contains("401") && err.contains("/kuma/metrics") && !err.contains("wrong-key-value"), "{err}");
    }

    #[test]
    fn defaults() {
        let s: Settings = serde_yaml::from_str("{url: 'https://kuma.example.invalid', api_key: k}").unwrap();
        assert!(!s.insecure && s.timeout_s.is_none());
        assert_eq!(s.list_max, 5);
        assert_eq!(UptimeKuma::new("k", s).unwrap().interval(), Duration::from_secs(60));
        let zero = serde_yaml::from_str("{url: 'https://kuma.example.invalid', api_key: k, timeout_s: 0}").unwrap();
        assert!(UptimeKuma::new("k", zero).is_err());
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
    }

    #[test]
    fn parses_samples_and_skips_comments_and_blank_lines() {
        let text = "# HELP monitor_status Monitor Status\n# TYPE monitor_status gauge\n\nmonitor_status{monitor_name=\"Web\",monitor_port=\"null\"} 1\nprocess_cpu_seconds_total 12.5 1761400000000\n";
        let got = parse(text).unwrap();
        assert_eq!(
            got,
            [
                Sample {
                    name: "monitor_status".into(),
                    labels: labels(&[("monitor_name", "Web"), ("monitor_port", "null")]),
                    value: 1.0
                },
                Sample { name: "process_cpu_seconds_total".into(), labels: BTreeMap::new(), value: 12.5 },
            ]
        );
    }

    #[test]
    fn label_values_unescape_quotes_backslashes_and_newlines() {
        let text = r#"m{a="say \"hi\"",b="C:\\dir",c="two\nlines",d="x,y}{=",e=""} -1"#;
        let got = parse(text).unwrap();
        assert_eq!(
            got[0].labels,
            labels(&[("a", "say \"hi\""), ("b", "C:\\dir"), ("c", "two\nlines"), ("d", "x,y}{="), ("e", "")])
        );
        assert_eq!(got[0].value, -1.0);
    }

    #[test]
    fn special_values_and_a_trailing_comma_parse() {
        let got = parse("a{x=\"1\",} NaN\nb +Inf\nc{} -Inf\n").unwrap();
        assert!(got[0].value.is_nan());
        assert_eq!(got[0].labels, labels(&[("x", "1")]));
        assert_eq!(got[1].value, f64::INFINITY);
        assert_eq!(got[2].value, f64::NEG_INFINITY);
    }

    #[test]
    fn malformed_lines_name_the_line_but_not_its_content() {
        for bad in [
            "ok 1\nm{a=\"https://user:secretpw@x.example.invalid\" 1",
            "ok 1\nm{a=secretpw} 1",
            "ok 1\nm{a=\"secretpw\"} notanumber",
            "ok 1\nm{a=\"secretpw\"}",
        ] {
            let err = parse(bad).unwrap_err().to_string();
            assert!(err.contains("line 2") && !err.contains("secretpw"), "{err}");
        }
    }
}
