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
    pub fields: Vec<Field>,
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
}

impl Field {
    fn validate(&self) -> anyhow::Result<()> {
        if self.over.is_some() && self.kind != FieldKind::Number {
            anyhow::bail!("field {}: over needs kind number", self.key);
        }
        Ok(())
    }
}

/// `number` needs a JSON number (or a boolean, read as 1/0) and `text` a JSON string; anything else fails the field.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    #[default]
    Number,
    Text,
}

pub struct HttpJson {
    name: String,
    interval: Duration,
    url: reqwest::Url,
    client: reqwest::Client,
    fields: Vec<Field>,
}

impl HttpJson {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        settings.fields.iter().try_for_each(Field::validate)?;
        let client = crate::http::client(crate::http::DEFAULT_TIMEOUT.min(interval), &settings.headers, None, false)?;
        Ok(Self {
            name: name.to_owned(),
            interval,
            url: crate::http::base_url(&settings.url)?,
            client,
            fields: settings.fields,
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
    }
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
            match extract(&body, field) {
                Ok(value) => partial.ok([(field.key.clone(), value)]),
                Err(e) => partial.fail(&field.key, &e),
            }
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
