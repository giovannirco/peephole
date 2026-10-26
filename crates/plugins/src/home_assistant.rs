//! Home Assistant REST API, verified against the developer documentation
//! (<https://contributors.home-assistant.io/core/internals/rest-api/>, formerly
//! developers.home-assistant.io/docs/api/rest): every request sends
//! `Authorization: Bearer <long-lived token>`.
//!
//! - `GET /api/` answers `{"message": "API running."}`.
//! - `GET /api/config`: only `version` and the length of `components` are read; the
//!   location fields are never deserialized.
//! - `GET /api/states` lists state objects `{entity_id, state, attributes, last_changed, …}`;
//!   `GET /api/states/<entity_id>` returns one or 404.
//!
//! `person.*` and `device_tracker.*` states can be a zone name, so they only give `on`
//! (home or not), never the state text.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use peephole_core::{Plugin, Value};
use serde::Deserialize;

/// No `Debug`: the struct holds the access token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Base URL, e.g. `http://homeassistant.example.invalid:8123`.
    pub url: String,
    /// Long-lived access token, sent as `Authorization: Bearer`; never logged.
    pub token: String,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub insecure: bool,
    /// One `GET /api/states` for the counts and the entities; off = one request per entity, no counts.
    #[serde(default = "default_true")]
    pub summary: bool,
    #[serde(default)]
    pub entities: Vec<Entity>,
}

fn default_interval() -> u64 {
    30
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entity {
    pub id: String,
    /// Store key segment; defaults to the sanitized entity id.
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Deserialize)]
struct Config {
    version: String,
    #[serde(default)]
    components: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct State {
    entity_id: String,
    state: String,
    #[serde(default)]
    attributes: Attributes,
    #[serde(default)]
    last_changed: Option<String>,
}

#[derive(Default, Deserialize)]
struct Attributes {
    #[serde(default)]
    unit_of_measurement: Option<String>,
}

impl State {
    fn domain(&self) -> &str {
        domain(&self.entity_id)
    }
}

fn domain(entity_id: &str) -> &str {
    entity_id.split_once('.').map_or(entity_id, |(d, _)| d)
}

type Values = Vec<(String, Value)>;

struct Selected {
    id: String,
    key: String,
}

pub struct HomeAssistant {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    base: reqwest::Url,
    summary: bool,
    entities: Vec<Selected>,
}

impl HomeAssistant {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        let timeout = settings
            .timeout_s
            .map(|t| crate::period::seconds(t, "timeout_s"))
            .transpose()?
            .unwrap_or(crate::http::DEFAULT_TIMEOUT.min(interval));
        let entities = settings
            .entities
            .into_iter()
            .map(|e| Selected { key: crate::key::sanitize(e.key.as_deref().unwrap_or(&e.id)), id: e.id })
            .collect();
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(timeout, &Default::default(), Some(&settings.token), settings.insecure)?,
            base: crate::http::base_url(&settings.url)?,
            summary: settings.summary,
            entities,
        })
    }

    async fn json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> anyhow::Result<T> {
        let url = crate::http::join(&self.base, path);
        let path = url.path().to_owned();
        crate::http::get(&self.client, url).await?.json().await.map_err(|e| anyhow::anyhow!("GET {path}: {e}"))
    }

    async fn api(&self) -> anyhow::Result<Values> {
        crate::http::get(&self.client, crate::http::join(&self.base, "api/")).await?;
        Ok(vec![("up".into(), Value::Number(1.0))])
    }

    async fn config(&self) -> anyhow::Result<Values> {
        let c: Config = self.json("api/config").await?;
        Ok(vec![
            ("version".into(), Value::Text(c.version)),
            ("components".into(), Value::Number(c.components.len() as f64)),
        ])
    }
}

/// 1/0 for on/off, home/not_home, open/closed and playing/paused; any other person or tracker zone is not home.
fn on_state(domain: &str, state: &str) -> Option<f64> {
    match state {
        "on" | "home" | "open" | "playing" => Some(1.0),
        "off" | "not_home" | "closed" | "paused" => Some(0.0),
        _ if is_location(domain) && !matches!(state, "unavailable" | "unknown") => Some(0.0),
        _ => None,
    }
}

fn is_location(domain: &str) -> bool {
    matches!(domain, "person" | "device_tracker")
}

fn summary_values(states: &[State]) -> Values {
    let count = |pick: &dyn Fn(&State) -> bool| Value::Number(states.iter().filter(|s| pick(s)).count() as f64);
    let mut out = vec![
        ("entities_total".into(), Value::Number(states.len() as f64)),
        ("unavailable".into(), count(&|s| matches!(s.state.as_str(), "unavailable" | "unknown"))),
        ("lights_on".into(), count(&|s| s.domain() == "light" && s.state == "on")),
        ("switches_on".into(), count(&|s| s.domain() == "switch" && s.state == "on")),
        ("people_home".into(), count(&|s| s.domain() == "person" && s.state == "home")),
        ("automations".into(), count(&|s| s.domain() == "automation")),
        ("sensors".into(), count(&|s| s.domain() == "sensor")),
        ("binary_sensors_on".into(), count(&|s| s.domain() == "binary_sensor" && s.state == "on")),
        ("media_playing".into(), count(&|s| s.domain() == "media_player" && s.state == "playing")),
    ];
    let mut domains: BTreeMap<&str, usize> = BTreeMap::new();
    for s in states {
        *domains.entry(s.domain()).or_default() += 1;
    }
    for (d, n) in domains {
        out.push((format!("domain/{}/count", crate::key::sanitize(d)), Value::Number(n as f64)));
    }
    out
}

fn entity_values(key: &str, state: &State, now: DateTime<Utc>) -> Values {
    let mut out = Vec::new();
    let domain = state.domain();
    if let Ok(n) = state.state.parse::<f64>()
        && n.is_finite()
    {
        out.push((format!("entity/{key}/value"), Value::Number(n)));
    }
    if !is_location(domain) {
        out.push((format!("entity/{key}/text"), Value::Text(state.state.clone())));
    }
    if let Some(unit) = &state.attributes.unit_of_measurement {
        out.push((format!("entity/{key}/unit"), Value::Text(unit.clone())));
    }
    if let Some(changed) = state.last_changed.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok()) {
        let age = (now - changed.with_timezone(&Utc)).num_milliseconds() as f64 / 1000.0;
        out.push((format!("entity/{key}/age"), Value::Number(age.max(0.0).round())));
    }
    if let Some(on) = on_state(domain, &state.state) {
        out.push((format!("entity/{key}/on"), Value::Number(on)));
    }
    out
}

#[async_trait]
impl Plugin for HomeAssistant {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        let (api, config) = tokio::join!(self.api(), self.config());
        let up = api.is_ok();
        for (part, result) in [("api", api), ("config", config)] {
            match result {
                Ok(values) => partial.ok(values),
                Err(e) => partial.fail(part, &e),
            }
        }
        let now = Utc::now();
        let mut missing = 0;
        if self.summary {
            match self.json::<Vec<State>>("api/states").await {
                Ok(states) => {
                    partial.ok(summary_values(&states));
                    let by_id: BTreeMap<&str, &State> = states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
                    for selected in &self.entities {
                        match by_id.get(selected.id.as_str()) {
                            Some(state) => partial.ok(entity_values(&selected.key, state, now)),
                            None => {
                                missing += 1;
                                partial
                                    .fail(&format!("entity {}", selected.key), &anyhow::anyhow!("not in /api/states"));
                            }
                        }
                    }
                }
                Err(e) => partial.fail("states", &e),
            }
        } else {
            let fetched = futures::future::join_all(
                self.entities.iter().map(|e| async move { self.json::<State>(&format!("api/states/{}", e.id)).await }),
            )
            .await;
            for (selected, result) in self.entities.iter().zip(fetched) {
                match result {
                    Ok(state) => partial.ok(entity_values(&selected.key, &state, now)),
                    Err(e) => {
                        missing += 1;
                        partial.fail(&format!("entity {}", selected.key), &e);
                    }
                }
            }
        }
        let mut out = partial.finish()?;
        if !up {
            out.push(("up".into(), Value::Number(0.0)));
        }
        out.push(("_entities_missing".into(), Value::Number(missing as f64)));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const TOKEN: &str = "ha-token-not-real";

    fn ago(seconds: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(seconds)).to_rfc3339()
    }

    fn state(id: &str, value: &str, attributes: serde_json::Value, changed_ago: i64) -> serde_json::Value {
        json!({
            "entity_id": id, "state": value, "attributes": attributes,
            "last_changed": ago(changed_ago), "last_reported": ago(5), "last_updated": ago(5),
            "context": {"id": "01JX0000000000000000000000", "parent_id": null, "user_id": null}
        })
    }

    fn states() -> serde_json::Value {
        json!([
            state(
                "sensor.living_room_temperature",
                "21.5",
                json!({"unit_of_measurement": "°C", "device_class": "temperature", "friendly_name": "Living room"}),
                120
            ),
            state(
                "sensor.outdoor_humidity",
                "unavailable",
                json!({"unit_of_measurement": "%", "friendly_name": "Out"}),
                60
            ),
            state("sensor.washer_status", "idle", json!({"friendly_name": "Washer"}), 30),
            state("light.kitchen", "on", json!({"brightness": 200, "friendly_name": "Kitchen"}), 10),
            state("light.hall", "off", json!({"friendly_name": "Hall"}), 10),
            state("cover.garage", "closed", json!({"friendly_name": "Porch"}), 10),
            state("lock.back_door", "unknown", json!({"friendly_name": "Back door"}), 10),
            state("switch.heater", "on", json!({"friendly_name": "Heater"}), 10),
            state("binary_sensor.front_door", "on", json!({"device_class": "door", "friendly_name": "Front door"}), 10),
            state("binary_sensor.motion", "off", json!({"friendly_name": "Motion"}), 10),
            state("binary_sensor.window", "on", json!({"friendly_name": "Window"}), 10),
            state(
                "person.someone",
                "Secret Office",
                json!({"latitude": -23.5, "longitude": -46.6, "friendly_name": "Someone"}),
                10
            ),
            state("person.other", "home", json!({"friendly_name": "Other"}), 10),
            state("automation.lights_at_dusk", "on", json!({"friendly_name": "Dusk"}), 10),
            state("media_player.tv", "playing", json!({"friendly_name": "TV"}), 10),
            state("media_player.speaker", "paused", json!({"friendly_name": "Speaker"}), 10)
        ])
    }

    fn config() -> serde_json::Value {
        json!({
            "components": ["api", "http", "light", "sensor", "person"], "config_dir": "/config", "elevation": 760,
            "latitude": -23.5505, "longitude": -46.6333, "location_name": "Secret Home", "time_zone": "America/Sao_Paulo",
            "unit_system": {"length": "km", "mass": "g", "temperature": "°C", "volume": "L"},
            "version": "2026.10.1", "state": "RUNNING", "safe_mode": false, "external_url": null, "internal_url": null
        })
    }

    async fn mount(server: &MockServer, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(route.to_owned()))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn hass() -> MockServer {
        let server = MockServer::start().await;
        let ok = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
        mount(&server, "/api/", ok(json!({"message": "API running."}))).await;
        mount(&server, "/api/config", ok(config())).await;
        mount(&server, "/api/states", ok(states())).await;
        for entry in states().as_array().unwrap() {
            let id = entry["entity_id"].as_str().unwrap();
            mount(&server, &format!("/api/states/{id}"), ok(entry.clone())).await;
        }
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("401: Unauthorized"))
            .mount(&server)
            .await;
        server
    }

    const SELECTED: &str = r#"
entities:
  - {id: sensor.living_room_temperature, key: living_temp}
  - {id: binary_sensor.front_door}
  - {id: media_player.speaker}
  - {id: person.someone, key: someone}
  - {id: sensor.washer_status, key: washer}
  - {id: sensor.gone, key: gone}
"#;

    fn plugin(server: &MockServer, token: &str, extra: &str) -> HomeAssistant {
        let yaml = format!("url: {}\ntoken: {token}\n{extra}", server.uri());
        HomeAssistant::new("ha", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &HomeAssistant) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    fn n(got: &BTreeMap<String, Value>, key: &str) -> f64 {
        got.get(key).and_then(Value::as_f64).unwrap_or_else(|| panic!("{key} missing"))
    }

    #[tokio::test]
    async fn counts_the_house_from_one_states_call() {
        let server = hass().await;
        let got = poll(&plugin(&server, TOKEN, "")).await;
        assert_eq!(n(&got, "up"), 1.0);
        assert_eq!(got["version"], Value::Text("2026.10.1".into()));
        assert_eq!(n(&got, "components"), 5.0);
        assert_eq!(n(&got, "entities_total"), 16.0);
        assert_eq!(n(&got, "unavailable"), 2.0);
        assert_eq!(n(&got, "lights_on"), 1.0);
        assert_eq!(n(&got, "switches_on"), 1.0);
        assert_eq!(n(&got, "people_home"), 1.0);
        assert_eq!(n(&got, "automations"), 1.0);
        assert_eq!(n(&got, "sensors"), 3.0);
        assert_eq!(n(&got, "binary_sensors_on"), 2.0);
        assert_eq!(n(&got, "media_playing"), 1.0);
        assert_eq!(n(&got, "domain/light/count"), 2.0);
        assert_eq!(n(&got, "domain/cover/count"), 1.0);
        assert_eq!(n(&got, "domain/media_player/count"), 2.0);
        assert_eq!(n(&got, "_requests_ok"), 3.0);
        assert_eq!(n(&got, "_requests_failed"), 0.0);
    }

    #[tokio::test]
    async fn selected_entities_give_value_text_unit_age_and_on() {
        let server = hass().await;
        let got = poll(&plugin(&server, TOKEN, SELECTED)).await;
        assert_eq!(n(&got, "entity/living_temp/value"), 21.5);
        assert_eq!(got["entity/living_temp/text"], Value::Text("21.5".into()));
        assert_eq!(got["entity/living_temp/unit"], Value::Text("°C".into()));
        assert!((119.0..=125.0).contains(&n(&got, "entity/living_temp/age")));
        assert!(!got.contains_key("entity/living_temp/on"));
        assert_eq!(n(&got, "entity/binary_sensor.front_door/on"), 1.0);
        assert_eq!(n(&got, "entity/media_player.speaker/on"), 0.0);
        assert_eq!(got["entity/washer/text"], Value::Text("idle".into()));
        assert!(!got.contains_key("entity/washer/value") && !got.contains_key("entity/washer/unit"));
        assert!(!got.contains_key("entity/washer/on"));
        assert!(!got.keys().any(|k| k.starts_with("entity/gone/")));
        assert_eq!(n(&got, "_entities_missing"), 1.0);
        assert_eq!(n(&got, "_requests_failed"), 1.0);
    }

    #[tokio::test]
    async fn a_person_gives_home_or_away_never_the_zone_name() {
        let server = hass().await;
        let got = poll(&plugin(&server, TOKEN, SELECTED)).await;
        assert_eq!(n(&got, "entity/someone/on"), 0.0);
        assert!(!got.contains_key("entity/someone/text"));
        for (key, value) in &got {
            let text = format!("{key} {value:?}");
            assert!(!text.contains("Secret") && !text.contains("-23.5") && !text.contains("Sao_Paulo"), "{text}");
            assert!(!text.contains("Living room") && !text.contains(TOKEN), "{text}");
        }
    }

    #[tokio::test]
    async fn summary_off_fetches_each_entity_instead() {
        let server = hass().await;
        let got = poll(&plugin(&server, TOKEN, &format!("summary: false\n{SELECTED}"))).await;
        assert_eq!(n(&got, "entity/living_temp/value"), 21.5);
        assert_eq!(n(&got, "entity/binary_sensor.front_door/on"), 1.0);
        assert!(!got.contains_key("entities_total"));
        assert_eq!(n(&got, "_entities_missing"), 1.0);
        assert_eq!(n(&got, "_requests_ok"), 7.0);
        assert_eq!(n(&got, "_requests_failed"), 1.0);
        let requests = server.received_requests().await.unwrap();
        assert!(!requests.iter().any(|r| r.url.path() == "/api/states"));
    }

    #[tokio::test]
    async fn a_down_api_reports_up_zero_and_keeps_the_rest() {
        let server = MockServer::start().await;
        mount(&server, "/api/config", ResponseTemplate::new(200).set_body_json(config())).await;
        mount(&server, "/api/states", ResponseTemplate::new(200).set_body_json(states())).await;
        let got = poll(&plugin(&server, TOKEN, "")).await;
        assert_eq!(n(&got, "up"), 0.0);
        assert_eq!(n(&got, "entities_total"), 16.0);
    }

    #[tokio::test]
    async fn a_wrong_token_fails_the_poll_without_echoing_it() {
        let server = hass().await;
        let err = format!("{:#}", plugin(&server, "wrong-token-value", SELECTED).poll().await.unwrap_err());
        assert!(err.contains("401") && !err.contains("wrong-token-value"), "{err}");
    }

    #[test]
    fn on_states_and_keys() {
        assert_eq!(on_state("light", "on"), Some(1.0));
        assert_eq!(on_state("cover", "closed"), Some(0.0));
        assert_eq!(on_state("device_tracker", "not_home"), Some(0.0));
        assert_eq!(on_state("device_tracker", "Work"), Some(0.0));
        assert_eq!(on_state("media_player", "playing"), Some(1.0));
        assert_eq!(on_state("sensor", "idle"), None);
        assert_eq!(on_state("sensor", "unavailable"), None);
    }

    #[test]
    fn defaults() {
        let s: Settings = serde_yaml::from_str("{url: 'http://ha.example.invalid:8123', token: t}").unwrap();
        assert!(s.summary && !s.insecure && s.entities.is_empty() && s.timeout_s.is_none());
        assert_eq!(HomeAssistant::new("h", s).unwrap().interval(), Duration::from_secs(30));
    }
}
