use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use peephole_core::{Plugin, Value};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub url: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub items: Vec<Items>,
}

fn default_interval() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub key: String,
    /// RFC 6901 JSON pointer into the response body, e.g. `/data/0/temp`.
    pub pointer: String,
    #[serde(default)]
    pub kind: FieldKind,
    /// A second pointer: the field becomes `pointer / over` (0 when `over` reads 0). Numbers only.
    pub over: Option<String>,
    /// For `series`: a pointer into each array entry, e.g. `/1` for `[ts, value]` pairs or `/value` for objects.
    pub element: Option<String>,
    /// For `series`: keep only the last N points.
    pub last: Option<usize>,
}

/// One labelled set of keys per element of the array at `pointer`: `<prefix>/<label>/<field key>`,
/// plus `<prefix>_count` (elements seen).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Items {
    pub pointer: String,
    /// Pointer into each element to a string label.
    pub key: String,
    pub prefix: String,
    #[serde(default)]
    pub fields: Vec<Field>,
}

impl Field {
    fn validate(&self) -> anyhow::Result<()> {
        if self.over.is_some() && self.kind != FieldKind::Number {
            anyhow::bail!("field {}: over needs kind number", self.key);
        }
        if (self.element.is_some() || self.last.is_some()) && self.kind != FieldKind::Series {
            anyhow::bail!("field {}: element and last need kind series", self.key);
        }
        Ok(())
    }
}

/// `number` needs a JSON number (or a boolean, read as 1/0), `text` a JSON string and `series` a JSON array;
/// anything else fails the field. Series entries that are not numbers become NaN (a gap).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    #[default]
    Number,
    Text,
    Series,
}

pub struct HttpJson {
    name: String,
    interval: Duration,
    url: reqwest::Url,
    client: reqwest::Client,
    fields: Vec<Field>,
    items: Vec<Items>,
}

impl HttpJson {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        settings.fields.iter().chain(settings.items.iter().flat_map(|i| &i.fields)).try_for_each(Field::validate)?;
        let client = crate::http::client(crate::http::DEFAULT_TIMEOUT.min(interval), &settings.headers, None, false)?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            url: crate::http::base_url(&settings.url)?,
            client,
            fields: settings.fields,
            items: settings.items,
        })
    }
}

fn number(found: &serde_json::Value) -> Option<f64> {
    match found {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

fn at<'a>(body: &'a serde_json::Value, pointer: &str) -> anyhow::Result<&'a serde_json::Value> {
    body.pointer(pointer).ok_or_else(|| anyhow::anyhow!("pointer {pointer} not found"))
}

fn number_at(body: &serde_json::Value, pointer: &str) -> anyhow::Result<f64> {
    let found = at(body, pointer)?;
    number(found).ok_or_else(|| anyhow::anyhow!("pointer {pointer} holds {found}, not a Number"))
}

fn ratio(a: f64, b: f64) -> f64 {
    if b == 0.0 { 0.0 } else { a / b }
}

fn series_at(body: &serde_json::Value, field: &Field) -> anyhow::Result<Vec<f64>> {
    let found = at(body, &field.pointer)?;
    let entries =
        found.as_array().ok_or_else(|| anyhow::anyhow!("pointer {} holds {found}, not a Series", field.pointer))?;
    let points: Vec<f64> = entries
        .iter()
        .map(|entry| field.element.as_deref().map_or(Some(entry), |element| entry.pointer(element)))
        .map(|point| point.and_then(number).unwrap_or(f64::NAN))
        .collect();
    let skip = field.last.map_or(0, |last| points.len().saturating_sub(last));
    Ok(points[skip..].to_vec())
}

fn extract(body: &serde_json::Value, field: &Field) -> anyhow::Result<Value> {
    match field.kind {
        FieldKind::Number => match &field.over {
            Some(over) => Ok(Value::Number(ratio(number_at(body, &field.pointer)?, number_at(body, over)?))),
            None => number_at(body, &field.pointer).map(Value::Number),
        },
        FieldKind::Text => match at(body, &field.pointer)? {
            serde_json::Value::String(s) => Ok(Value::Text(s.clone())),
            other => anyhow::bail!("pointer {} holds {other}, not a Text", field.pointer),
        },
        FieldKind::Series => series_at(body, field).map(Value::Series),
    }
}

fn read(partial: &mut crate::partial::Partial, body: &serde_json::Value, field: &Field, key: String) {
    match extract(body, field) {
        Ok(value) => partial.ok([(key, value)]),
        Err(e) => partial.fail(&key, &e),
    }
}

fn array_at<'a>(body: &'a serde_json::Value, pointer: &str) -> anyhow::Result<&'a Vec<serde_json::Value>> {
    let found = at(body, pointer)?;
    found.as_array().ok_or_else(|| anyhow::anyhow!("pointer {pointer} holds {found}, not an array"))
}

fn label_at(element: &serde_json::Value, pointer: &str) -> anyhow::Result<String> {
    match at(element, pointer)? {
        serde_json::Value::String(label) => Ok(crate::key::sanitize(label)),
        other => anyhow::bail!("label {pointer} holds {other}, not a string"),
    }
}

fn read_items(partial: &mut crate::partial::Partial, body: &serde_json::Value, items: &Items) {
    let prefix = crate::key::sanitize(&items.prefix);
    let elements = match array_at(body, &items.pointer) {
        Ok(elements) => elements,
        Err(e) => return partial.fail(&prefix, &e),
    };
    for (index, element) in elements.iter().enumerate() {
        let label = match label_at(element, &items.key) {
            Ok(label) => label,
            Err(e) => {
                partial.fail(&format!("{prefix}[{index}]"), &e);
                continue;
            }
        };
        for field in &items.fields {
            read(partial, element, field, format!("{prefix}/{label}/{}", crate::key::sanitize(&field.key)));
        }
    }
    partial.ok([(format!("{prefix}_count"), Value::Number(elements.len() as f64))]);
}

#[async_trait]
impl Plugin for HttpJson {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let body: serde_json::Value = crate::http::get(&self.client, self.url.clone()).await?.json().await?;
        let mut partial = crate::partial::Partial::new(&self.name, "fields");
        for field in &self.fields {
            read(&mut partial, &body, field, field.key.clone());
        }
        for items in &self.items {
            read_items(&mut partial, &body, items);
        }
        partial.finish()
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const FIELDS: &str = r#"
fields:
  - {key: temp, pointer: /sensors/0/temp}
  - {key: room, pointer: /sensors/0/room, kind: text}
  - {key: missing, pointer: /sensors/9/temp}
  - {key: quoted, pointer: /sensors/0/quoted}
"#;

    async fn server(status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .and(header("x-token", "t0ken"))
            .respond_with(ResponseTemplate::new(status).set_body_json(serde_json::json!({
                "sensors": [{"temp": 21.5, "room": "Office", "quoted": "3"}]
            })))
            .mount(&server)
            .await;
        server
    }

    fn plugin(server: &MockServer, fields: &str) -> HttpJson {
        let yaml = format!("url: {}/status\nheaders: {{x-token: t0ken}}\n{fields}", server.uri());
        HttpJson::new("json", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reads_each_field_and_counts_the_failed_ones() {
        let server = server(200).await;
        let got: BTreeMap<_, _> = plugin(&server, FIELDS).poll().await.unwrap().into_iter().collect();
        assert_eq!(got["temp"], Value::Number(21.5));
        assert_eq!(got["room"], Value::Text("Office".into()));
        assert!(!got.contains_key("missing"));
        assert!(!got.contains_key("quoted"));
        assert_eq!(got["_fields_ok"], Value::Number(2.0));
        assert_eq!(got["_fields_failed"], Value::Number(2.0));
    }

    #[tokio::test]
    async fn every_field_failing_is_an_error_naming_them() {
        let server = server(200).await;
        let fields = "fields: [{key: missing, pointer: /nope}, {key: wrong, pointer: /sensors/0/room}]";
        let err = plugin(&server, fields).poll().await.unwrap_err().to_string();
        assert!(err.contains("missing") && err.contains("/nope") && err.contains("wrong"), "{err}");
    }

    #[tokio::test]
    async fn http_errors_fail_the_poll() {
        let server = server(503).await;
        let err = plugin(&server, FIELDS).poll().await.unwrap_err().to_string();
        assert!(err.contains("503"), "{err}");
    }

    async fn serve(body: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    async fn poll(body: serde_json::Value, settings: &str) -> BTreeMap<String, Value> {
        let server = serve(body).await;
        plugin(&server, settings).poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn booleans_read_as_one_and_zero_but_strings_still_fail_a_number() {
        let body = serde_json::json!({"internetUp": true, "wanDown": false, "quoted": "1"});
        let fields =
            "fields: [{key: up, pointer: /internetUp}, {key: down, pointer: /wanDown}, {key: q, pointer: /quoted}]";
        let got = poll(body, fields).await;
        assert_eq!(got["up"], Value::Number(1.0));
        assert_eq!(got["down"], Value::Number(0.0));
        assert!(!got.contains_key("q"));
        assert_eq!(got["_fields_failed"], Value::Number(1.0));
    }

    #[tokio::test]
    async fn over_divides_two_pointers_and_a_zero_divisor_reads_zero() {
        let body = serde_json::json!({"memoryUsed": 3, "memoryTotal": 4, "zero": 0});
        let fields = "fields: [{key: mem, pointer: /memoryUsed, over: /memoryTotal}, {key: z, pointer: /memoryUsed, over: /zero}, {key: gone, pointer: /memoryUsed, over: /nope}]";
        let got = poll(body, fields).await;
        assert_eq!(got["mem"], Value::Number(0.75));
        assert_eq!(got["z"], Value::Number(0.0));
        assert!(!got.contains_key("gone"));
        assert_eq!(got["_fields_failed"], Value::Number(1.0));
    }

    fn series(got: &BTreeMap<String, Value>, key: &str) -> Vec<f64> {
        got[key].as_series().unwrap_or_else(|| panic!("{key} is not a series: {:?}", got[key])).to_vec()
    }

    #[tokio::test]
    async fn series_read_numbers_pairs_and_objects_with_nan_for_bad_entries_and_last() {
        let body = serde_json::json!({
            "plain": [1, 2.5, 3],
            "gpus": [{"history": [[1792984863, 0], [1792984923, 0.4], [1792984983, "x"], [1792985043, 0.9]]}],
            "objects": [{"value": 5}, {"value": null}, {"other": 1}, {"value": 7}]
        });
        let fields = r#"
fields:
  - {key: plain, pointer: /plain, kind: series}
  - {key: pairs, pointer: /gpus/0/history, kind: series, element: /1}
  - {key: tail, pointer: /gpus/0/history, kind: series, element: /1, last: 2}
  - {key: objects, pointer: /objects, kind: series, element: /value}
  - {key: notarray, pointer: /objects/0, kind: series}
"#;
        let got = poll(body, fields).await;
        assert_eq!(got["plain"], Value::Series(vec![1.0, 2.5, 3.0]));
        let pairs = series(&got, "pairs");
        assert_eq!(pairs.len(), 4);
        assert_eq!((pairs[0], pairs[1], pairs[3]), (0.0, 0.4, 0.9));
        assert!(pairs[2].is_nan());
        let tail = series(&got, "tail");
        assert!(tail.len() == 2 && tail[0].is_nan() && tail[1] == 0.9, "{tail:?}");
        let objects = series(&got, "objects");
        assert!(objects[0] == 5.0 && objects[1].is_nan() && objects[2].is_nan() && objects[3] == 7.0, "{objects:?}");
        assert!(!got.contains_key("notarray"));
        assert_eq!(got["_fields_failed"], Value::Number(1.0));
    }

    #[test]
    fn element_or_last_without_kind_series_is_refused() {
        let err = build("fields: [{key: cpu, pointer: /a, element: /1}]").err().unwrap().to_string();
        assert!(err.contains("cpu") && err.contains("series"), "{err}");
        let err = build("fields: [{key: cpu, pointer: /a, last: 3}]").err().unwrap().to_string();
        assert!(err.contains("cpu") && err.contains("series"), "{err}");
    }

    fn hosts() -> serde_json::Value {
        serde_json::json!({"hosts": [
            {"name": "Compute A", "up": true, "cpu": 0.0987, "memoryUsed": 3, "memoryTotal": 4,
             "history": [[1792984863, 0.1], [1792984923, 0.2]]},
            {"name": "Compute B", "up": false, "cpu": 0.5},
            {"name": 7, "up": true, "cpu": 1}
        ]})
    }

    const HOSTS: &str = r#"
items:
  - pointer: /hosts
    key: /name
    prefix: host
    fields:
      - {key: cpu, pointer: /cpu}
      - {key: mem, pointer: /memoryUsed, over: /memoryTotal}
      - {key: up, pointer: /up}
      - {key: hist, pointer: /history, kind: series, element: /1}
"#;

    #[tokio::test]
    async fn items_label_each_element_and_a_missing_field_fails_only_itself() {
        let got = poll(hosts(), HOSTS).await;
        assert_eq!(got["host/Compute-A/cpu"], Value::Number(0.0987));
        assert_eq!(got["host/Compute-A/mem"], Value::Number(0.75));
        assert_eq!(got["host/Compute-A/up"], Value::Number(1.0));
        assert_eq!(got["host/Compute-A/hist"], Value::Series(vec![0.1, 0.2]));
        assert_eq!(got["host/Compute-B/cpu"], Value::Number(0.5));
        assert_eq!(got["host/Compute-B/up"], Value::Number(0.0));
        assert!(!got.contains_key("host/Compute-B/mem") && !got.contains_key("host/Compute-B/hist"));
        assert!(!got.keys().any(|k| k.contains('7')), "{:?}", got.keys());
        assert_eq!(got["host_count"], Value::Number(3.0));
        assert_eq!(got["_fields_failed"], Value::Number(3.0));
    }

    #[tokio::test]
    async fn items_over_a_non_array_fail_as_one_part_and_keep_the_top_level_fields() {
        let settings =
            format!("fields: [{{key: first, pointer: /hosts/0/cpu}}]\n{}", HOSTS.replace("/hosts", "/hosts/0"));
        let got = poll(hosts(), &settings).await;
        assert_eq!(got["first"], Value::Number(0.0987));
        assert!(!got.keys().any(|k| k.starts_with("host")), "{:?}", got.keys());
        assert_eq!(got["_fields_failed"], Value::Number(1.0));
        let server = serve(hosts()).await;
        let err = plugin(&server, &HOSTS.replace("/hosts", "/nope")).poll().await.unwrap_err().to_string();
        assert!(err.contains("host") && err.contains("/nope"), "{err}");
    }

    fn build(yaml: &str) -> anyhow::Result<HttpJson> {
        HttpJson::new("j", serde_yaml::from_str(&format!("url: 'http://example.invalid'\n{yaml}")).unwrap())
    }

    #[test]
    fn over_on_a_text_field_is_refused_naming_the_field() {
        let err = build("fields: [{key: room, pointer: /a, kind: text, over: /b}]").err().unwrap().to_string();
        assert!(err.contains("room") && err.contains("over"), "{err}");
    }

    #[test]
    fn defaults_to_a_minute() {
        let s: Settings = serde_yaml::from_str("{url: 'http://example.invalid', fields: []}").unwrap();
        assert_eq!(HttpJson::new("j", s).unwrap().interval(), Duration::from_secs(60));
    }
}
