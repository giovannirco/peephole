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
}

/// `number` needs a JSON number and `text` a JSON string; anything else fails the field.
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

fn extract(body: &serde_json::Value, field: &Field) -> anyhow::Result<Value> {
    let found = body.pointer(&field.pointer).ok_or_else(|| anyhow::anyhow!("pointer {} not found", field.pointer))?;
    match (field.kind, found) {
        (FieldKind::Number, serde_json::Value::Number(n)) => {
            n.as_f64().map(Value::Number).ok_or_else(|| anyhow::anyhow!("{} is not an f64", field.pointer))
        }
        (FieldKind::Text, serde_json::Value::String(s)) => Ok(Value::Text(s.clone())),
        (kind, other) => anyhow::bail!("pointer {} holds {other}, not a {kind:?}", field.pointer),
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

    #[test]
    fn defaults_to_a_minute() {
        let s: Settings = serde_yaml::from_str("{url: 'http://example.invalid', fields: []}").unwrap();
        assert_eq!(HttpJson::new("j", s).unwrap().interval(), Duration::from_secs(60));
    }
}
