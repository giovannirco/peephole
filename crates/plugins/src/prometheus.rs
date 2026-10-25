use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Base URL; `/api/v1/query` and `/api/v1/query_range` are appended.
    pub url: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub bearer_token: Option<String>,
    pub queries: Vec<Query>,
}

fn default_interval() -> u64 {
    60
}
fn default_timeout() -> u64 {
    10
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub key: String,
    pub expr: String,
    /// Label whose value names each sample when the result has several.
    #[serde(default)]
    pub label: Option<String>,
    /// Label value to display name.
    #[serde(default)]
    pub rename: BTreeMap<String, String>,
    #[serde(default)]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub span_s: u64,
    pub step_s: u64,
}

pub struct Prometheus {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    query_url: reqwest::Url,
    range_url: reqwest::Url,
    queries: Vec<Query>,
}

impl Prometheus {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let base = crate::http::base_url(&settings.url)?;
        for q in &settings.queries {
            if let Some(r) = q.range {
                anyhow::ensure!(r.step_s > 0, "query {:?}: range.step_s must be greater than 0", q.key);
                anyhow::ensure!(r.span_s >= r.step_s, "query {:?}: range.span_s must be at least step_s", q.key);
            }
        }
        let timeout = crate::period::seconds(settings.timeout_s, "timeout_s")?;
        Ok(Self {
            name: name.to_owned(),
            interval: crate::period::seconds(settings.interval_s, "interval_s")?,
            client: crate::http::client(timeout, &settings.headers, settings.bearer_token.as_deref(), false)?,
            query_url: crate::http::join(&base, "api/v1/query"),
            range_url: crate::http::join(&base, "api/v1/query_range"),
            queries: settings.queries,
        })
    }

    async fn run(&self, query: &Query, now: i64) -> anyhow::Result<Vec<(String, Value)>> {
        match query.range {
            None => {
                let mut url = self.query_url.clone();
                url.query_pairs_mut().append_pair("query", &query.expr);
                match self.fetch(url).await? {
                    Data::Vector(samples) => {
                        let named = samples.into_iter().map(|s| (s.metric, parse_number(&s.value.1)));
                        name_results(
                            query,
                            named.map(|(m, v)| Ok((m, Value::Number(v?)))).collect::<anyhow::Result<_>>()?,
                        )
                    }
                    Data::Scalar((_, v)) => Ok(vec![(query.key.clone(), Value::Number(parse_number(&v)?))]),
                    Data::Matrix(_) => anyhow::bail!("instant query returned a matrix; set range for range queries"),
                }
            }
            Some(range) => {
                let step = range.step_s as i64;
                let end = now - now.rem_euclid(step);
                let start = end - range.span_s as i64;
                let mut url = self.range_url.clone();
                url.query_pairs_mut()
                    .append_pair("query", &query.expr)
                    .append_pair("start", &start.to_string())
                    .append_pair("end", &end.to_string())
                    .append_pair("step", &step.to_string());
                match self.fetch(url).await? {
                    Data::Matrix(series) => {
                        let slots = (range.span_s / range.step_s) as usize + 1;
                        let named = series
                            .into_iter()
                            .map(|s| Ok((s.metric, Value::Series(grid(&s.values, start, step, slots)?))))
                            .collect::<anyhow::Result<_>>()?;
                        name_results(query, named)
                    }
                    _ => anyhow::bail!("range query did not return a matrix"),
                }
            }
        }
    }

    async fn fetch(&self, url: reqwest::Url) -> anyhow::Result<Data> {
        let response = self.client.get(url).send().await?;
        let status = response.status();
        let text = response.text().await?;
        let body: ApiResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("HTTP {status}: response is not a Prometheus API body ({e})"))?;
        match (body.status.as_str(), body.data) {
            ("success", Some(data)) if status.is_success() => Ok(data),
            _ => anyhow::bail!(
                "HTTP {status}: {}: {}",
                body.error_type.as_deref().unwrap_or("error"),
                body.error.as_deref().unwrap_or("no error message")
            ),
        }
    }
}

#[derive(Deserialize)]
struct ApiResponse {
    status: String,
    #[serde(default)]
    data: Option<Data>,
    #[serde(default, rename = "errorType")]
    error_type: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "resultType", content = "result", rename_all = "lowercase")]
enum Data {
    Vector(Vec<Sample>),
    Matrix(Vec<RangeSeries>),
    Scalar((f64, String)),
}

#[derive(Deserialize)]
struct Sample {
    #[serde(default)]
    metric: BTreeMap<String, String>,
    value: (f64, String),
}

#[derive(Deserialize)]
struct RangeSeries {
    #[serde(default)]
    metric: BTreeMap<String, String>,
    values: Vec<(f64, String)>,
}

fn parse_number(raw: &str) -> anyhow::Result<f64> {
    raw.parse().map_err(|_| anyhow::anyhow!("sample value {raw:?} is not a number"))
}

fn grid(values: &[(f64, String)], start: i64, step: i64, slots: usize) -> anyhow::Result<Vec<f64>> {
    let mut out = vec![f64::NAN; slots];
    for (ts, raw) in values {
        let slot = ((ts - start as f64) / step as f64).round();
        if slot >= 0.0 && (slot as usize) < slots {
            out[slot as usize] = parse_number(raw)?;
        }
    }
    Ok(out)
}

fn name_results(
    query: &Query,
    results: Vec<(BTreeMap<String, String>, Value)>,
) -> anyhow::Result<Vec<(String, Value)>> {
    anyhow::ensure!(!results.is_empty(), "no samples");
    let Some(label) = &query.label else {
        anyhow::ensure!(
            results.len() == 1,
            "{} samples but no label to tell them apart; set label on query {:?}",
            results.len(),
            query.key
        );
        let (_, value) = results.into_iter().next().expect("exactly one result");
        return Ok(vec![(query.key.clone(), value)]);
    };
    results
        .into_iter()
        .map(|(metric, value)| {
            let raw = metric.get(label).ok_or_else(|| anyhow::anyhow!("a sample has no label {label:?}"))?;
            let shown = query.rename.get(raw).unwrap_or(raw);
            Ok((format!("{}/{}", query.key, crate::key::sanitize(shown)), value))
        })
        .collect()
}

#[async_trait]
impl Plugin for Prometheus {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let now = chrono::Utc::now().timestamp();
        let results = futures::future::join_all(self.queries.iter().map(|q| self.run(q, now))).await;
        let mut partial = crate::partial::Partial::new(&self.name, "queries");
        for (query, result) in self.queries.iter().zip(results) {
            match result {
                Ok(values) => partial.ok(values),
                Err(e) => partial.fail(&query.key, &e),
            }
        }
        partial.finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use super::*;

    fn vector(samples: serde_json::Value) -> serde_json::Value {
        json!({"status": "success", "data": {"resultType": "vector", "result": samples}})
    }

    async fn instant(server: &MockServer, expr: &str, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/prometheus/api/v1/query"))
            .and(query_param("query", expr))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn plugin(server: &MockServer, queries: &str) -> Prometheus {
        let yaml = format!("url: {}/prometheus\nbearer_token: s3cret\nqueries: {queries}", server.uri());
        Prometheus::new("prom", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &Prometheus) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn single_sample_becomes_the_key_and_sends_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/prometheus/api/v1/query"))
            .and(header("authorization", "Bearer s3cret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vector(json!([
                {"metric": {}, "value": [1761400000.5, "0.42"]}
            ]))))
            .mount(&server)
            .await;
        let got = poll(&plugin(&server, "[{key: cpu, expr: 'avg(rate(x[5m]))'}]")).await;
        assert_eq!(got["cpu"], Value::Number(0.42));
        assert_eq!(got["_queries_ok"], Value::Number(1.0));
        assert_eq!(got["_queries_failed"], Value::Number(0.0));
    }

    #[tokio::test]
    async fn labelled_vector_fans_out_with_renames() {
        let server = MockServer::start().await;
        instant(
            &server,
            "up",
            vector(json!([
                {"metric": {"instance": "node-a.example.invalid:9100"}, "value": [1, "1"]},
                {"metric": {"instance": "node b"}, "value": [1, "0"]}
            ])),
        )
        .await;
        let got = poll(&plugin(
            &server,
            "[{key: up, expr: up, label: instance, rename: {'node-a.example.invalid:9100': alpha}}]",
        ))
        .await;
        assert_eq!(got["up/alpha"], Value::Number(1.0));
        assert_eq!(got["up/node-b"], Value::Number(0.0));
    }

    #[tokio::test]
    async fn nan_and_infinities_parse_and_scalars_are_numbers() {
        let server = MockServer::start().await;
        instant(&server, "a", vector(json!([{"metric": {}, "value": [1, "NaN"]}]))).await;
        instant(&server, "b", vector(json!([{"metric": {}, "value": [1, "+Inf"]}]))).await;
        instant(&server, "c", json!({"status": "success", "data": {"resultType": "scalar", "result": [1, "-Inf"]}}))
            .await;
        let got = poll(&plugin(&server, "[{key: a, expr: a}, {key: b, expr: b}, {key: c, expr: c}]")).await;
        assert!(got["a"].as_f64().unwrap().is_nan());
        assert_eq!(got["b"], Value::Number(f64::INFINITY));
        assert_eq!(got["c"], Value::Number(f64::NEG_INFINITY));
    }

    #[tokio::test]
    async fn one_failing_query_does_not_sink_the_others() {
        let server = MockServer::start().await;
        instant(&server, "good", vector(json!([{"metric": {}, "value": [1, "7"]}]))).await;
        instant(
            &server,
            "many",
            vector(json!([
                {"metric": {"job": "a"}, "value": [1, "1"]},
                {"metric": {"job": "b"}, "value": [1, "2"]}
            ])),
        )
        .await;
        instant(&server, "none", vector(json!([]))).await;
        Mock::given(method("GET"))
            .and(path("/prometheus/api/v1/query"))
            .and(query_param("query", "bad("))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "status": "error", "errorType": "bad_data", "error": "parse error"
            })))
            .mount(&server)
            .await;
        let queries =
            "[{key: good, expr: good}, {key: many, expr: many}, {key: none, expr: none}, {key: bad, expr: 'bad('}]";
        let got = poll(&plugin(&server, queries)).await;
        assert_eq!(got["good"], Value::Number(7.0));
        assert_eq!(got["_queries_ok"], Value::Number(1.0));
        assert_eq!(got["_queries_failed"], Value::Number(3.0));
        assert!(!got.keys().any(|k| k.starts_with("many") || k.starts_with("bad") || k.starts_with("none")));
    }

    #[tokio::test]
    async fn every_query_failing_is_an_error_with_the_reasons() {
        let server = MockServer::start().await;
        instant(
            &server,
            "many",
            vector(json!([
                {"metric": {"job": "a"}, "value": [1, "1"]},
                {"metric": {"job": "b"}, "value": [1, "2"]}
            ])),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/prometheus/api/v1/query"))
            .and(query_param("query", "bad("))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "status": "error", "errorType": "bad_data", "error": "parse error"
            })))
            .mount(&server)
            .await;
        let err = plugin(&server, "[{key: many, expr: many}, {key: bad, expr: 'bad('}]").poll().await.unwrap_err();
        let err = err.to_string();
        assert!(err.contains("many") && err.contains("label"), "{err}");
        assert!(err.contains("bad_data") && err.contains("parse error"), "{err}");
    }

    struct Matrix;

    impl Respond for Matrix {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let q: BTreeMap<String, String> = request.url.query_pairs().into_owned().collect();
            let start: f64 = q["start"].parse().unwrap();
            let end: f64 = q["end"].parse().unwrap();
            let step: f64 = q["step"].parse().unwrap();
            assert_eq!(end - start, 240.0);
            let at = |i: f64, v: &str| json!([start + i * step, v]);
            ResponseTemplate::new(200).set_body_json(
                json!({"status": "success", "data": {"resultType": "matrix", "result": [
                    {"metric": {"host": "a"}, "values": [at(0.0, "1"), at(1.0, "2"), at(3.0, "4")]},
                    {"metric": {"host": "b"}, "values": [at(4.0, "9")]}
                ]}}),
            )
        }
    }

    #[tokio::test]
    async fn range_queries_become_series_on_the_step_grid() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/prometheus/api/v1/query_range"))
            .respond_with(Matrix)
            .mount(&server)
            .await;
        let queries = "[{key: load, expr: x, label: host, range: {span_s: 240, step_s: 60}}, {key: solo, expr: y, range: {span_s: 240, step_s: 60}}]";
        let got = poll(&plugin(&server, queries)).await;
        let a = got["load/a"].as_series().unwrap();
        assert_eq!(a.len(), 5);
        assert_eq!(a[..2], [1.0, 2.0]);
        assert!(a[2].is_nan() && a[4].is_nan());
        assert_eq!(a[3], 4.0);
        let b = got["load/b"].as_series().unwrap();
        assert!(b[..4].iter().all(|v| v.is_nan()));
        assert_eq!(b[4], 9.0);
        assert!(!got.contains_key("solo"));
        assert_eq!(got["_queries_failed"], Value::Number(1.0));
    }

    #[test]
    fn invalid_ranges_and_defaults() {
        let s: Settings = serde_yaml::from_str("{url: 'http://example.invalid', queries: []}").unwrap();
        let p = Prometheus::new("p", s).unwrap();
        assert_eq!(p.interval(), Duration::from_secs(60));
        for range in ["{span_s: 60, step_s: 0}", "{span_s: 30, step_s: 60}"] {
            let yaml = format!("{{url: 'http://example.invalid', queries: [{{key: slowq, expr: e, range: {range}}}]}}");
            let err = Prometheus::new("p", serde_yaml::from_str(&yaml).unwrap()).err().unwrap();
            assert!(err.to_string().contains("slowq"), "{err}");
        }
    }
}
