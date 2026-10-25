use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use peephole_core::{Event, Plugin, Value};
use serde::Deserialize;

const API: &str = "proxy/protect/integration/v1";

/// No `Debug`: the struct holds the console's API key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Console base URL, e.g. `https://nvr.example.invalid`.
    pub url: String,
    /// Sent as `X-API-KEY`; never logged.
    pub api_key: String,
    /// Accept the console's self-signed certificate.
    #[serde(default)]
    pub insecure: bool,
    #[serde(default = "default_interval")]
    pub interval_s: u64,
    #[serde(default = "default_true")]
    pub snapshots: bool,
    #[serde(default)]
    pub snapshot_max: SnapshotMax,
    /// Page to show when an event arrives; no page, no event stream.
    #[serde(default)]
    pub event_page: Option<String>,
    #[serde(default = "default_hold")]
    pub hold_s: u64,
    #[serde(default = "default_event_types")]
    pub event_types: Vec<String>,
}

fn default_interval() -> u64 {
    30
}
fn default_true() -> bool {
    true
}
fn default_hold() -> u64 {
    20
}
fn default_event_types() -> Vec<String> {
    ["ring", "motion", "smartDetectZone"].map(String::from).to_vec()
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotMax {
    pub w: u32,
    pub h: u32,
}

impl Default for SnapshotMax {
    fn default() -> Self {
        Self { w: 320, h: 170 }
    }
}

/// `GET /meta/info`. Guessed: the version field is `applicationVersion`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaInfo {
    application_version: String,
}

/// One entry of `GET /cameras`. Guessed: `lastMotion` is epoch milliseconds
/// (as in the console's private API) and may be absent or null.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Camera {
    id: String,
    name: String,
    state: String,
    #[serde(default)]
    last_motion: Option<i64>,
}

impl Camera {
    fn online(&self) -> bool {
        self.state == "CONNECTED"
    }
}

type Values = Vec<(String, Value)>;

pub struct UnifiProtect {
    name: String,
    interval: Duration,
    client: reqwest::Client,
    api: reqwest::Url,
    snapshots: bool,
    snapshot_max: SnapshotMax,
    camera_names: Arc<Mutex<HashMap<String, String>>>,
    events: Option<Arc<EventSource>>,
}

impl UnifiProtect {
    pub fn new(name: &str, settings: Settings) -> anyhow::Result<Self> {
        let interval = crate::period::seconds(settings.interval_s, "interval_s")?;
        anyhow::ensure!(
            settings.snapshot_max.w > 0 && settings.snapshot_max.h > 0,
            "snapshot_max must be at least 1x1"
        );
        let headers = [("X-API-KEY".to_owned(), settings.api_key.clone())].into();
        let base = crate::http::base_url(&settings.url)?;
        let camera_names = Arc::new(Mutex::new(HashMap::new()));
        let events = match settings.event_page {
            Some(page) => Some(Arc::new(EventSource {
                plugin: name.to_owned(),
                url: events_url(&base)?,
                api_key: settings.api_key.clone(),
                insecure: settings.insecure,
                page,
                hold: crate::period::seconds(settings.hold_s, "hold_s")?,
                types: settings.event_types,
                camera_names: camera_names.clone(),
            })),
            None => None,
        };
        Ok(Self {
            name: name.to_owned(),
            interval,
            client: crate::http::client(crate::http::DEFAULT_TIMEOUT.min(interval), &headers, None, settings.insecure)?,
            api: crate::http::join(&base, API),
            snapshots: settings.snapshots,
            snapshot_max: settings.snapshot_max,
            camera_names,
            events,
        })
    }

    fn url(&self, path: &str) -> reqwest::Url {
        crate::http::join(&self.api, path)
    }

    async fn version(&self) -> anyhow::Result<Values> {
        let meta: MetaInfo = crate::http::get(&self.client, self.url("meta/info")).await?.json().await?;
        Ok(vec![("version".into(), Value::Text(meta.application_version))])
    }

    async fn cameras(&self) -> anyhow::Result<Vec<Camera>> {
        Ok(crate::http::get(&self.client, self.url("cameras")).await?.json().await?)
    }

    async fn snapshot(&self, camera: &Camera) -> anyhow::Result<Values> {
        let mut url = self.url(&format!("cameras/{}/snapshot", camera.id));
        url.query_pairs_mut().append_pair("highQuality", "false");
        let bytes = crate::http::get(&self.client, url).await?.bytes().await?;
        let decoded = image::load_from_memory(&bytes)?;
        let max = self.snapshot_max;
        let fitted = if decoded.width() > max.w || decoded.height() > max.h {
            decoded.resize(max.w, max.h, image::imageops::FilterType::Triangle)
        } else {
            decoded
        };
        let rgba = fitted.to_rgba8();
        let (width, height) = rgba.dimensions();
        let key = format!("snapshot/{}", crate::key::sanitize(&camera.name));
        Ok(vec![(key, Value::Image { width, height, rgba: Arc::new(rgba.into_raw()) })])
    }
}

fn camera_values(cameras: &[Camera], now_ms: i64) -> Values {
    let mut out = Vec::new();
    for camera in cameras {
        let key = crate::key::sanitize(&camera.name);
        out.push((format!("camera/{key}/online"), Value::Number(if camera.online() { 1.0 } else { 0.0 })));
        if let Some(ms) = camera.last_motion {
            let age = ((now_ms - ms) as f64 / 1000.0).max(0.0);
            out.push((format!("camera/{key}/last_motion_age"), Value::Number(age.round())));
        }
    }
    let online = cameras.iter().filter(|c| c.online()).count();
    out.push(("cameras_online".into(), Value::Number(online as f64)));
    out.push(("cameras_total".into(), Value::Number(cameras.len() as f64)));
    out
}

#[async_trait]
impl Plugin for UnifiProtect {
    fn name(&self) -> &str {
        &self.name
    }
    fn interval(&self) -> Duration {
        self.interval
    }
    fn events(&self) -> Option<BoxStream<'static, Event>> {
        use futures::StreamExt;

        let source = self.events.clone()?;
        let state = (source, None, FIRST_BACKOFF);
        let stream = futures::stream::unfold(state, |(source, mut socket, mut backoff)| async move {
            let event = source.next(&mut socket, &mut backoff).await;
            Some((event, (source, socket, backoff)))
        });
        Some(stream.boxed())
    }

    async fn poll(&self) -> anyhow::Result<Vec<(String, Value)>> {
        let (version, cameras) = tokio::join!(self.version(), self.cameras());
        let mut partial = crate::partial::Partial::new(&self.name, "requests");
        match version {
            Ok(values) => partial.ok(values),
            Err(e) => partial.fail("meta/info", &e),
        }
        let cameras = match cameras {
            Ok(cameras) => cameras,
            Err(e) => {
                partial.fail("cameras", &e);
                return partial.finish();
            }
        };
        *self.camera_names.lock().expect("camera names poisoned") =
            cameras.iter().map(|c| (c.id.clone(), c.name.clone())).collect();
        partial.ok(camera_values(&cameras, chrono::Utc::now().timestamp_millis()));
        if self.snapshots {
            let online: Vec<&Camera> = cameras.iter().filter(|c| c.online()).collect();
            let shots = futures::future::join_all(online.iter().map(|c| self.snapshot(c))).await;
            for (camera, shot) in online.iter().zip(shots) {
                match shot {
                    Ok(values) => partial.ok(values),
                    Err(e) => partial.fail(&format!("snapshot {}", crate::key::sanitize(&camera.name)), &e),
                }
            }
        }
        partial.finish()
    }
}

const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// A configured event out of one `subscribe/events` message, as `(type, camera)`.
/// Guessed shape: `{"type": "add", "item": {"modelKey": "event", "type": "ring",
/// "device": "<camera id>"}}`; only `add` counts (an `update` repeats an event).
fn parse_event(text: &str, types: &[String], names: &HashMap<String, String>) -> Option<(String, String)> {
    let message: serde_json::Value = serde_json::from_str(text).ok()?;
    if message.get("type")?.as_str()? != "add" {
        return None;
    }
    let item = message.get("item")?;
    if item.get("modelKey")?.as_str()? != "event" {
        return None;
    }
    let kind = item.get("type")?.as_str()?;
    if !types.iter().any(|t| t == kind) {
        return None;
    }
    let camera = item.get("device").or_else(|| item.get("camera"))?.as_str()?;
    let shown = names.get(camera).cloned().unwrap_or_else(|| camera.to_owned());
    Some((kind.to_owned(), shown))
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Everything the event stream needs, owned so the stream can outlive `poll`.
struct EventSource {
    plugin: String,
    url: String,
    api_key: String,
    insecure: bool,
    page: String,
    hold: Duration,
    types: Vec<String>,
    camera_names: Arc<Mutex<HashMap<String, String>>>,
}

impl EventSource {
    async fn connect(&self) -> anyhow::Result<Socket> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::HeaderValue;

        let mut request = self.url.as_str().into_client_request()?;
        let mut key = HeaderValue::from_str(&self.api_key)?;
        key.set_sensitive(true);
        request.headers_mut().insert("X-API-KEY", key);
        let connector = if self.insecure {
            Some(tokio_tungstenite::Connector::Rustls(crate::insecure_tls::client_config()?))
        } else {
            None
        };
        let (socket, _) = tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector).await?;
        Ok(socket)
    }

    fn event(&self, text: &str) -> Option<Event> {
        let names = self.camera_names.lock().expect("camera names poisoned");
        let (kind, camera) = parse_event(text, &self.types, &names)?;
        Some(Event { page: self.page.clone(), hold: self.hold, reason: format!("{kind} {camera}") })
    }

    async fn next(&self, socket: &mut Option<Socket>, backoff: &mut Duration) -> Event {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        loop {
            let Some(open) = socket.as_mut() else {
                match self.connect().await {
                    Ok(connected) => *socket = Some(connected),
                    Err(e) => self.wait(backoff, &format!("connect failed: {e}")).await,
                }
                continue;
            };
            match open.next().await {
                Some(Ok(Message::Text(text))) => {
                    *backoff = FIRST_BACKOFF;
                    if let Some(event) = self.event(&text) {
                        return event;
                    }
                }
                Some(Ok(_)) => *backoff = FIRST_BACKOFF,
                Some(Err(e)) => {
                    *socket = None;
                    self.wait(backoff, &format!("stream error: {e}")).await;
                }
                None => {
                    *socket = None;
                    self.wait(backoff, "stream closed").await;
                }
            }
        }
    }

    async fn wait(&self, backoff: &mut Duration, why: &str) {
        tracing::warn!(plugin = %self.plugin, retry_in_s = backoff.as_secs(), "event stream: {why}");
        tokio::time::sleep(*backoff).await;
        *backoff = next_backoff(*backoff);
    }
}

fn events_url(base: &reqwest::Url) -> anyhow::Result<String> {
    let mut url = crate::http::join(base, &format!("{API}/subscribe/events"));
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).map_err(|()| anyhow::anyhow!("cannot derive the events URL from {base}"))?;
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const KEY: &str = "test-key-not-real";

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    async fn mount(server: &MockServer, route: &str, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/{API}/{route}")))
            .and(header("x-api-key", KEY))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn console(snapshot: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        mount(&server, "meta/info", ResponseTemplate::new(200).set_body_json(json!({"applicationVersion": "6.1.0"})))
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let cameras = json!([
            {"id": "cam1", "name": "Front door", "state": "CONNECTED", "isMicEnabled": true, "lastMotion": now_ms - 90_000},
            {"id": "cam2", "name": "Garage/Side", "state": "DISCONNECTED", "isMicEnabled": false}
        ]);
        mount(&server, "cameras", ResponseTemplate::new(200).set_body_json(cameras)).await;
        Mock::given(method("GET"))
            .and(path(format!("/{API}/cameras/cam1/snapshot")))
            .and(query_param("highQuality", "false"))
            .and(header("x-api-key", KEY))
            .respond_with(snapshot)
            .mount(&server)
            .await;
        server
    }

    fn plugin(server: &MockServer, extra: &str) -> UnifiProtect {
        let yaml = format!("url: {}\napi_key: {KEY}\n{extra}", server.uri());
        UnifiProtect::new("protect", serde_yaml::from_str(&yaml).unwrap()).unwrap()
    }

    async fn poll(p: &UnifiProtect) -> BTreeMap<String, Value> {
        p.poll().await.unwrap().into_iter().collect()
    }

    #[tokio::test]
    async fn reports_cameras_and_a_resized_snapshot() {
        let server = console(ResponseTemplate::new(200).set_body_raw(jpeg(640, 240), "image/jpeg")).await;
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["version"], Value::Text("6.1.0".into()));
        assert_eq!(got["cameras_total"], Value::Number(2.0));
        assert_eq!(got["cameras_online"], Value::Number(1.0));
        assert_eq!(got["camera/Front-door/online"], Value::Number(1.0));
        assert_eq!(got["camera/Garage-Side/online"], Value::Number(0.0));
        let age = got["camera/Front-door/last_motion_age"].as_f64().unwrap();
        assert!((89.0..=95.0).contains(&age), "{age}");
        assert!(!got.contains_key("camera/Garage-Side/last_motion_age"));
        let Value::Image { width, height, rgba } = &got["snapshot/Front-door"] else { panic!("no snapshot") };
        assert_eq!((*width, *height), (320, 120));
        assert_eq!(rgba.len(), 320 * 120 * 4);
        assert!(rgba[0] > 150 && rgba[1] < 80 && rgba[3] == 255);
        assert!(!got.contains_key("snapshot/Garage-Side"));
    }

    #[tokio::test]
    async fn a_broken_snapshot_does_not_sink_the_camera_list() {
        let server = console(ResponseTemplate::new(200).set_body_raw(b"not a jpeg".to_vec(), "image/jpeg")).await;
        let got = poll(&plugin(&server, "")).await;
        assert_eq!(got["cameras_online"], Value::Number(1.0));
        assert!(!got.contains_key("snapshot/Front-door"));
        assert_eq!(got["_requests_failed"], Value::Number(1.0));
    }

    #[tokio::test]
    async fn snapshots_can_be_turned_off() {
        let server = console(ResponseTemplate::new(500)).await;
        let got = poll(&plugin(&server, "snapshots: false")).await;
        assert!(!got.keys().any(|k| k.starts_with("snapshot/")));
        assert_eq!(got["_requests_failed"], Value::Number(0.0));
    }

    #[tokio::test]
    async fn a_wrong_key_fails_the_poll_without_echoing_it() {
        let server = console(ResponseTemplate::new(200)).await;
        let yaml = format!("url: {}\napi_key: wrong-key-value", server.uri());
        let p = UnifiProtect::new("protect", serde_yaml::from_str(&yaml).unwrap()).unwrap();
        let err = format!("{:#}", p.poll().await.unwrap_err());
        assert!(err.contains("404") && !err.contains("wrong-key-value"), "{err}");
    }

    #[test]
    fn backoff_doubles_up_to_a_minute() {
        let mut b = FIRST_BACKOFF;
        let mut seen = Vec::new();
        for _ in 0..8 {
            b = next_backoff(b);
            seen.push(b.as_secs());
        }
        assert_eq!(seen, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn parses_only_new_events_of_configured_types() {
        let types = default_event_types();
        let names = HashMap::from([("cam1".to_owned(), "Front door".to_owned())]);
        let msg = |kind: &str, event: &str, device: &str| {
            json!({"type": kind, "item": {"id": "e1", "modelKey": "event", "type": event, "device": device}})
                .to_string()
        };
        assert_eq!(
            parse_event(&msg("add", "ring", "cam1"), &types, &names),
            Some(("ring".into(), "Front door".into()))
        );
        assert_eq!(parse_event(&msg("add", "motion", "cam9"), &types, &names), Some(("motion".into(), "cam9".into())));
        assert_eq!(parse_event(&msg("update", "ring", "cam1"), &types, &names), None);
        assert_eq!(parse_event(&msg("add", "smartAudioDetect", "cam1"), &types, &names), None);
        let device_event = json!({"type": "add", "item": {"id": "cam1", "modelKey": "camera", "type": "ring"}});
        assert_eq!(parse_event(&device_event.to_string(), &types, &names), None);
        assert_eq!(parse_event("not json", &types, &names), None);
    }

    #[test]
    fn no_event_page_means_no_stream() {
        let s: Settings = serde_yaml::from_str("{url: 'https://nvr.example.invalid', api_key: k}").unwrap();
        assert!(UnifiProtect::new("p", s).unwrap().events().is_none());
    }

    #[tokio::test]
    #[expect(clippy::result_large_err, reason = "tungstenite's handshake callback fixes the error type")]
    async fn stream_reconnects_and_yields_configured_events() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::handshake::server::{Request as WsRequest, Response as WsResponse};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let check = |req: &WsRequest, resp: WsResponse| {
                    assert_eq!(req.uri().path(), format!("/{API}/subscribe/events"));
                    assert_eq!(req.headers()["x-api-key"], KEY);
                    Ok(resp)
                };
                let mut ws = tokio_tungstenite::accept_hdr_async(tcp, check).await.unwrap();
                if attempt == 0 {
                    ws.close(None).await.unwrap();
                    continue;
                }
                let update = json!({"type": "update", "item": {"modelKey": "event", "type": "ring", "device": "cam1"}});
                let ring = json!({"type": "add", "item": {"modelKey": "event", "type": "ring", "device": "cam1"}});
                ws.send(Message::text(update.to_string())).await.unwrap();
                ws.send(Message::text(ring.to_string())).await.unwrap();
                ws.next().await;
            }
        });
        let yaml = format!(
            "url: http://{addr}
api_key: {KEY}
event_page: Door
hold_s: 15"
        );
        let p = UnifiProtect::new("protect", serde_yaml::from_str(&yaml).unwrap()).unwrap();
        let mut events = p.events().unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), events.next()).await.unwrap().unwrap();
        assert_eq!(event, Event { page: "Door".into(), hold: Duration::from_secs(15), reason: "ring cam1".into() });
        drop(events);
        server.abort();
    }

    #[test]
    fn defaults() {
        let s: Settings = serde_yaml::from_str("{url: 'https://nvr.example.invalid', api_key: k}").unwrap();
        assert!(!s.insecure && s.snapshots && s.event_page.is_none());
        assert_eq!(s.snapshot_max, SnapshotMax { w: 320, h: 170 });
        assert_eq!(s.hold_s, 20);
        assert_eq!(s.event_types, ["ring", "motion", "smartDetectZone"]);
        assert_eq!(UnifiProtect::new("p", s).unwrap().interval(), Duration::from_secs(30));
    }
}
