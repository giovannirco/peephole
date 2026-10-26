use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use peephole_core::scheduler::{self, PollTriggers, Scheduler};
use peephole_core::{Config, Plugin, Registry, Store, Value};
use peephole_hub::routes::router;
use peephole_hub::settings::{ApiToken, Settings};
use peephole_hub::state::{self, AppState, Hub};
use serde_json::{Value as Json, json};
use tower::ServiceExt;

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const CONFIG: &str = r#"
hub: {title: Test hub, cycle_s: 0, stale_s: 60}
plugins:
  - kind: clock
  - kind: demo
    name: d
    values: {n: 1}
pages:
  - name: alpha
    title: Alpha
    cards: [{x: 0, y: 0, w: 100, h: 50}]
  - name: beta
"#;

fn settings(demo: bool, firmware_dir: Option<PathBuf>, public_url: Option<&str>) -> Settings {
    Settings {
        config: None,
        listen: "127.0.0.1:0".parse().unwrap(),
        demo,
        state_file: None,
        firmware_dir,
        public_url: public_url.map(str::to_owned),
        api_token: None,
        set_variables: Vec::new(),
    }
}

fn hub_with(settings: Settings, yaml: &str) -> AppState {
    hub_from(settings, Config::parse(yaml, |_| None).unwrap(), Store::new(), PollTriggers::default())
}

fn hub_from(settings: Settings, cfg: Config, store: Store, polls: PollTriggers) -> AppState {
    let plugins: Vec<Arc<dyn Plugin>> = cfg.plugins.iter().map(|e| peephole_plugins::build(e).unwrap()).collect();
    let registry = Registry::new(cfg.hub.cycle_s);
    Hub::new(&settings, cfg, store, registry, state::intervals(&plugins), polls)
}

fn running_hub(yaml: &str) -> (AppState, Scheduler) {
    let cfg = Config::parse(yaml, |_| None).unwrap();
    let plugins = cfg.plugins.iter().map(|e| peephole_plugins::build(e).unwrap()).collect();
    let store = Store::new();
    let (events, _) = tokio::sync::broadcast::channel(4);
    let scheduler = scheduler::run(plugins, store.clone(), events);
    (hub_from(settings(false, None, None), cfg, store, scheduler.triggers()), scheduler)
}

async fn polled_at(
    state: &AppState,
    plugin: &str,
    after: Option<chrono::DateTime<chrono::Utc>>,
) -> chrono::DateTime<chrono::Utc> {
    let key = format!("{plugin}/_polled_at");
    for _ in 0..500 {
        if let Some(s) = state.store.get(&key).filter(|s| after.is_none_or(|a| s.updated_at > a)) {
            return s.updated_at;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    panic!("{plugin} was not polled");
}

fn hub(demo: bool) -> AppState {
    hub_with(settings(demo, None, None), CONFIG)
}

async fn call(app: &Router, method: Method, uri: &str, body: Option<Json>) -> (StatusCode, HeaderMap, Bytes) {
    call_as(app, method, uri, body, None).await
}

async fn call_as(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Json>,
    authorization: Option<&str>,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(value) = authorization {
        req = req.header("authorization", value);
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    call(app, Method::GET, uri, None).await
}

async fn post(app: &Router, uri: &str, body: Option<Json>) -> (StatusCode, Json) {
    let (status, _, bytes) = call(app, Method::POST, uri, body).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
}

fn json_of(bytes: &Bytes) -> Json {
    serde_json::from_slice(bytes).unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(bytes)))
}

fn png_size(bytes: &[u8]) -> (u32, u32) {
    assert_eq!(bytes[..8], PNG_SIGNATURE, "not a PNG");
    let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    (w, h)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).unwrap_or_else(|| panic!("missing header {name}")).to_str().unwrap()
}

async fn register(app: &Router, id: &str) {
    let (status, _, _) = get(app, &format!("/frame.png?id={id}")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn healthz_counts_plugins_devices_and_pages() {
    let app = router(hub(false));
    register(&app, "m1").await;
    let (status, _, body) = get(&app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json_of(&body), json!({"ok": true, "plugins": 2, "devices": 1, "pages": 2}));
}

#[tokio::test]
async fn frame_registers_the_device_and_returns_a_png_of_the_requested_size() {
    let app = router(hub(false));
    let (status, headers, body) = get(&app, "/frame.png?id=m1&w=64&h=32").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "content-type"), "image/png");
    assert_eq!(header(&headers, "cache-control"), "no-store");
    assert_eq!(header(&headers, "x-peephole-page"), "alpha");
    assert_eq!(png_size(&body), (64, 32));
    let (_, _, body) = get(&app, "/api/devices/m1").await;
    let dev = json_of(&body);
    assert_eq!(dev["width"], 64);
    assert_eq!(dev["frames"], 1);
}

#[tokio::test]
async fn frame_defaults_to_320_by_170() {
    let app = router(hub(false));
    let (_, _, body) = get(&app, "/frame.png?id=m1").await;
    assert_eq!(png_size(&body), (320, 170));
}

#[tokio::test]
async fn frame_requires_an_id_and_sane_dimensions() {
    let app = router(hub(false));
    for uri in
        ["/frame.png", "/frame.png?id=", "/frame.png?id=m&w=0", "/frame.png?id=m&w=100000", "/frame.png?id=m&h=x"]
    {
        let (status, _, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert!(json_of(&body)["error"].is_string(), "{uri}");
    }
}

#[tokio::test]
async fn frame_without_pages_serves_a_notice() {
    let app = router(hub_with(settings(false, None, None), "hub: {title: Empty}\n"));
    let (status, headers, body) = get(&app, "/frame.png?id=m1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "x-peephole-page"), "notice");
    assert_eq!(png_size(&body), (320, 170));
}

#[tokio::test]
async fn next_twice_wraps_back_to_the_first_page() {
    let app = router(hub(false));
    register(&app, "m1").await;
    let (status, dev) = post(&app, "/api/devices/m1/next", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["page"], 1);
    let (_, headers, _) = get(&app, "/frame.png?id=m1").await;
    assert_eq!(header(&headers, "x-peephole-page"), "beta");
    let (_, dev) = post(&app, "/api/devices/m1/next", None).await;
    assert_eq!(dev["page"], 0);
}

#[tokio::test]
async fn prev_wraps_to_the_last_page() {
    let app = router(hub(false));
    register(&app, "m1").await;
    let (status, dev) = post(&app, "/api/devices/m1/prev", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["page"], 1);
}

#[tokio::test]
async fn set_page_by_name_and_unknown_page_is_404() {
    let app = router(hub(false));
    register(&app, "m1").await;
    let (status, dev) = post(&app, "/api/devices/m1/page", Some(json!({"name": "beta"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["page"], 1);
    let (status, body) = post(&app, "/api/devices/m1/page", Some(json!({"name": "nope"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("nope"));
}

#[tokio::test]
async fn message_is_stored_and_over_200_chars_is_rejected() {
    let app = router(hub(false));
    register(&app, "m1").await;
    let (status, dev) = post(&app, "/api/devices/m1/message", Some(json!({"text": "é".repeat(200)}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["message"], "é".repeat(200));
    let (status, body) = post(&app, "/api/devices/m1/message", Some(json!({"text": "x".repeat(201)}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn name_and_cycle_update_the_device() {
    let app = router(hub(false));
    register(&app, "m1").await;
    let (status, dev) = post(&app, "/api/devices/m1/name", Some(json!({"name": "Kitchen"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["name"], "Kitchen");
    let (status, dev) = post(&app, "/api/devices/m1/cycle", Some(json!({"cycle_s": 30}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["cycle_s"], 30);
}

#[tokio::test]
async fn malformed_bodies_are_400_json() {
    let app = router(hub(false));
    register(&app, "m1").await;
    let (status, body) = post(&app, "/api/devices/m1/message", Some(json!({"wrong": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn unknown_device_is_404() {
    let app = router(hub(false));
    let (status, _, body) = get(&app, "/api/devices/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json_of(&body)["error"].is_string());
    for action in ["next", "prev"] {
        let (status, body) = post(&app, &format!("/api/devices/ghost/{action}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{action}");
        assert!(body["error"].is_string());
    }
    let (status, _) = post(&app, "/api/devices/ghost/message", Some(json!({"text": "hi"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn demo_mode_forbids_every_post() {
    let app = router(hub(true));
    register(&app, "m1").await;
    for (action, body) in [
        ("next", None),
        ("prev", None),
        ("page", Some(json!({"name": "beta"}))),
        ("message", Some(json!({"text": "hi"}))),
        ("name", Some(json!({"name": "x"}))),
        ("cycle", Some(json!({"cycle_s": 5}))),
        ("pages", Some(json!({"pages": ["beta"]}))),
    ] {
        let (status, body) = post(&app, &format!("/api/devices/m1/{action}"), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{action}");
        assert!(body["error"].is_string(), "{action}");
    }
    let (status, _) = post(&app, "/api/plugins/d/poll", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, _, body) = get(&app, "/api/devices/m1").await;
    assert_eq!(json_of(&body)["page"], 0);
}

#[tokio::test]
async fn page_png_renders_without_touching_devices() {
    let app = router(hub(false));
    let (status, headers, body) = get(&app, "/pages/beta.png?w=640&h=340").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&headers, "content-type"), "image/png");
    assert_eq!(png_size(&body), (640, 340));
    let (_, _, body) = get(&app, "/api/devices").await;
    assert_eq!(json_of(&body), json!([]));
    let (status, _, body) = get(&app, "/pages/nope.png").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json_of(&body)["error"].is_string());
    let (status, _, _) = get(&app, "/pages/beta").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_pages_lists_each_page() {
    let app = router(hub(false));
    let (status, _, body) = get(&app, "/api/pages").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_of(&body),
        json!([
            {"name": "alpha", "title": "Alpha", "width": 320, "height": 170, "cards": 1},
            {"name": "beta", "title": null, "width": 320, "height": 170, "cards": 0}
        ])
    );
}

#[tokio::test]
async fn api_values_summarises_the_store() {
    let state = hub(false);
    state.store.set("d/n", Value::Number(1.5));
    state.store.set("d/s", Value::Series(vec![1.0, 2.0]));
    state.store.set("d/t", Value::Text("hi".into()));
    state.store.set("d/img", Value::Image { width: 4, height: 2, rgba: vec![0; 32].into() });
    let app = router(state);
    let (status, _, body) = get(&app, "/api/values").await;
    assert_eq!(status, StatusCode::OK);
    let v = json_of(&body);
    assert_eq!(v["d/n"]["value"], 1.5);
    assert_eq!(v["d/s"]["value"], json!([1.0, 2.0]));
    assert_eq!(v["d/t"]["value"], "hi");
    assert_eq!(v["d/img"]["value"], json!({"image": [4, 2]}));
    assert!(v["d/n"]["updated_at"].is_string());
    assert_eq!(v["d/n"]["age_s"], 0);
}

#[tokio::test]
async fn api_config_describes_the_hub() {
    let app = router(hub(true));
    let (_, _, body) = get(&app, "/api/config").await;
    assert_eq!(
        json_of(&body),
        json!({
            "title": "Test hub", "cycle_s": 0, "stale_s": 60, "demo": true, "auth": false,
            "plugins": [{"name": "clock", "kind": "clock"}, {"name": "d", "kind": "demo"}]
        })
    );
}

#[tokio::test]
async fn manifest_lists_the_newest_firmware_per_board() {
    let dir = tempfile::tempdir().unwrap();
    for f in ["tdisplay-s3-0.9.0.factory.bin", "tdisplay-s3-0.10.0.factory.bin", "notes.txt", "other-1.0.0.factory.bin"]
    {
        std::fs::write(dir.path().join(f), b"fw").unwrap();
    }
    let app = router(hub_with(settings(false, Some(dir.path().to_path_buf()), None), CONFIG));
    let (status, _, body) = get(&app, "/flash/manifest.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json_of(&body),
        json!({
            "name": "Peephole",
            "version": "0.10.0",
            "new_install_prompt_erase": true,
            "builds": [{"chipFamily": "ESP32-S3", "parts": [{"path": "/firmware/tdisplay-s3-0.10.0.factory.bin", "offset": 0}]}]
        })
    );
    let (status, _, body) = get(&app, "/firmware/tdisplay-s3-0.10.0.factory.bin").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"fw");
}

#[tokio::test]
async fn manifest_is_404_without_a_firmware_dir_or_files() {
    let app = router(hub(false));
    let (status, _, body) = get(&app, "/flash/manifest.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json_of(&body)["error"].is_string());
    let (status, _, body) = get(&app, "/firmware/tdisplay-s3-0.1.0.factory.bin").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json_of(&body)["error"].is_string());
    let empty = tempfile::tempdir().unwrap();
    let app = router(hub_with(settings(false, Some(empty.path().to_path_buf()), None), CONFIG));
    let (status, _, _) = get(&app, "/flash/manifest.json").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn flash_page_uses_esp_web_tools_and_shows_the_public_url() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("tdisplay-s3-0.1.0.factory.bin"), b"fw").unwrap();
    let state =
        hub_with(settings(false, Some(dir.path().to_path_buf()), Some("https://peephole.example.invalid")), CONFIG);
    let app = router(state);
    let (status, headers, body) = get(&app, "/flash/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(header(&headers, "content-type").starts_with("text/html"));
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("https://unpkg.com/esp-web-tools@10/dist/web/install-button.js?module"));
    assert!(html.contains(r#"<esp-web-install-button manifest="/flash/manifest.json">"#));
    assert!(html.contains("https://peephole.example.invalid"));
    assert!(!html.contains("No firmware"));
}

#[tokio::test]
async fn flash_page_says_when_no_firmware_is_present() {
    let app = router(hub(false));
    let (status, _, body) = get(&app, "/flash/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8(body.to_vec()).unwrap().contains("No firmware"));
}

#[tokio::test]
async fn index_lists_every_page_and_marks_demo_mode() {
    let app = router(hub(false));
    let (status, headers, body) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(header(&headers, "content-type").starts_with("text/html"));
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Test hub"));
    assert!(html.contains(r#"/pages/alpha.png?w=640&amp;h=340"#), "{html}");
    assert!(html.contains(r#"/pages/beta.png?w=640&amp;h=340"#));
    assert!(html.contains(r#"href="/flash/""#));
    assert!(html.contains("<th>Pages</th>"));
    assert!(!html.contains(">DEMO<"));
    let (_, _, body) = get(&router(hub(true)), "/").await;
    assert!(String::from_utf8(body.to_vec()).unwrap().contains(">DEMO<"));
}

#[tokio::test]
async fn index_escapes_page_names() {
    let yaml = "hub: {title: \"<b>t</b>\"}\npages: [{name: \"a<script>\"}]\n";
    let app = router(hub_with(settings(false, None, None), yaml));
    let (_, _, body) = get(&app, "/").await;
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(!html.contains("a<script>"));
    assert!(!html.contains("<b>t</b>"));
    assert!(html.contains("a&lt;script&gt;"));
}

#[tokio::test]
async fn unknown_routes_are_404_json() {
    let app = router(hub(false));
    let (status, _, body) = get(&app, "/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(json_of(&body)["error"].is_string());
}

const SLOW: &str = "plugins: [{kind: demo, name: d, interval_s: 3600, values: {n: 1}}]\n";

#[tokio::test]
async fn poll_now_is_202_and_polls_the_plugin_again() {
    let (state, scheduler) = running_hub(SLOW);
    let first = polled_at(&state, "d", None).await;
    let app = router(state.clone());
    let (status, body) = post(&app, "/api/plugins/d/poll", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body, json!({"polled": "d"}));
    polled_at(&state, "d", Some(first)).await;
    scheduler.abort();
}

#[tokio::test]
async fn poll_now_for_an_unknown_plugin_is_404() {
    let (state, scheduler) = running_hub(SLOW);
    let (status, body) = post(&router(state), "/api/plugins/nope/poll", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("nope"));
    scheduler.abort();
}

const FOUR: &str = "hub: {cycle_s: 0}\npages: [{name: a}, {name: b}, {name: c}, {name: d}]\n";

#[tokio::test]
async fn a_page_list_limits_next_prev_and_set_page() {
    let app = router(hub_with(settings(false, None, None), FOUR));
    register(&app, "m1").await;
    let (status, dev) = post(&app, "/api/devices/m1/pages", Some(json!({"pages": ["b", "d"]}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["pages"], json!(["b", "d"]));
    assert_eq!(dev["page"], 1);
    let (_, dev) = post(&app, "/api/devices/m1/next", None).await;
    assert_eq!(dev["page"], 3);
    let (_, dev) = post(&app, "/api/devices/m1/next", None).await;
    assert_eq!(dev["page"], 1);
    let (_, dev) = post(&app, "/api/devices/m1/prev", None).await;
    assert_eq!(dev["page"], 3);
    let (status, body) = post(&app, "/api/devices/m1/page", Some(json!({"name": "a"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("\"a\""));
    let (_, _, body) = get(&app, "/api/devices").await;
    assert_eq!(json_of(&body)[0]["pages"], json!(["b", "d"]));
}

#[tokio::test]
async fn an_empty_page_list_means_every_page() {
    let app = router(hub_with(settings(false, None, None), FOUR));
    register(&app, "m1").await;
    post(&app, "/api/devices/m1/pages", Some(json!({"pages": ["c"]}))).await;
    let (status, dev) = post(&app, "/api/devices/m1/pages", Some(json!({"pages": []}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["pages"], json!([]));
    let (_, dev) = post(&app, "/api/devices/m1/next", None).await;
    assert_eq!(dev["page"], 3);
}

#[tokio::test]
async fn a_page_list_with_unknown_names_is_400_naming_them() {
    let app = router(hub_with(settings(false, None, None), FOUR));
    register(&app, "m1").await;
    let (status, body) = post(&app, "/api/devices/m1/pages", Some(json!({"pages": ["a", "zz"]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("zz"));
    let (status, _) = post(&app, "/api/devices/ghost/pages", Some(json!({"pages": []}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

const TOKEN: &str = "tok-0123456789";

fn token_settings(demo: bool) -> Settings {
    Settings { api_token: Some(ApiToken::new(TOKEN)), ..settings(demo, None, None) }
}

async fn post_as(app: &Router, uri: &str, body: Option<Json>, authorization: Option<&str>) -> (StatusCode, Json) {
    let (status, _, bytes) = call_as(app, Method::POST, uri, body, authorization).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Json::Null))
}

#[tokio::test]
async fn with_a_token_every_post_needs_the_bearer_header() {
    let app = router(hub_with(token_settings(false), CONFIG));
    register(&app, "m1").await;
    let wrong = format!("Bearer {TOKEN}x");
    let short = format!("Bearer {}", &TOKEN[1..]);
    for authorization in [None, Some("Bearer nope"), Some(wrong.as_str()), Some(short.as_str()), Some(TOKEN)] {
        let (status, body) = post_as(&app, "/api/devices/m1/next", None, authorization).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        assert!(body["error"].is_string(), "{authorization:?}");
    }
    let (_, _, body) = get(&app, "/api/devices/m1").await;
    assert_eq!(json_of(&body)["page"], 0);
    let bearer = format!("Bearer {TOKEN}");
    let (status, dev) = post_as(&app, "/api/devices/m1/next", None, Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dev["page"], 1);
}

#[tokio::test]
async fn with_a_token_poll_now_is_accepted_with_the_bearer_header() {
    let (running, scheduler) = running_hub(SLOW);
    let state = hub_from(token_settings(false), running.cfg.clone(), running.store.clone(), running.polls.clone());
    let app = router(state);
    let (status, _) = post_as(&app, "/api/plugins/d/poll", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = post_as(&app, "/api/plugins/d/poll", None, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    scheduler.abort();
}

#[tokio::test]
async fn with_a_token_reads_stay_open_and_config_reports_auth() {
    let app = router(hub_with(token_settings(false), CONFIG));
    for uri in ["/", "/api/pages", "/api/devices", "/api/values", "/healthz"] {
        let (status, _, _) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
    }
    let (_, _, body) = get(&app, "/api/config").await;
    assert_eq!(json_of(&body)["auth"], true);
}

#[tokio::test]
async fn demo_mode_answers_403_even_with_a_token() {
    let app = router(hub_with(token_settings(true), CONFIG));
    register(&app, "m1").await;
    for authorization in [None, Some(format!("Bearer {TOKEN}"))] {
        let (status, _) = post_as(&app, "/api/devices/m1/next", None, authorization.as_deref()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{authorization:?}");
    }
}

const SENTINEL: &str = "SENTINEL-c2VjcmV0";

const SECRET_CONFIG: &str = r#"
hub: {title: Secret hub, cycle_s: 7, stale_s: 90}
plugins:
  - kind: prometheus
    name: p
    url: "http://u:${T}@prom.invalid:9090"
    bearer_token: ${T}
    headers: {X-Scope-OrgID: "${T}"}
    queries: [{key: up, expr: up}]
  - kind: clock
    tz: ${TZ_UNSET:-UTC}
pages: [{name: a}, {name: b}]
"#;

fn secret_hub() -> AppState {
    let cfg = Config::parse(SECRET_CONFIG, |k| (k == "T").then(|| SENTINEL.to_owned())).unwrap();
    let store = Store::new();
    store.set("p/_ok", Value::Number(0.0));
    store.set(
        "p/_error",
        Value::Text(format!("error sending request for url (http://u:{SENTINEL}@prom.invalid:9090/api)")),
    );
    let mut s = token_settings(false);
    s.config = Some(PathBuf::from("/etc/peephole.yaml"));
    s.public_url = Some("https://hub.example.invalid".into());
    s.set_variables = vec!["PEEPHOLE_CONFIG", "PEEPHOLE_API_TOKEN"];
    hub_from(s, cfg, store, PollTriggers::default())
}

#[tokio::test]
async fn no_expanded_secret_reaches_any_settings_view() {
    let app = router(secret_hub());
    for uri in ["/api/settings", "/api/plugins", "/settings", "/api/config"] {
        let (status, _, body) = get(&app, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains(SENTINEL), "{uri} leaks the secret: {text}");
        assert!(!text.contains(TOKEN), "{uri} leaks the API token");
    }
}

#[tokio::test]
async fn api_settings_shows_the_hub_its_variables_and_redacted_plugins() {
    let app = router(secret_hub());
    let (_, _, body) = get(&app, "/api/settings").await;
    let v = json_of(&body);
    assert_eq!(
        v["hub"],
        json!({
            "title": "Secret hub", "cycle_s": 7, "stale_s": 90, "demo": false, "auth": true,
            "listen": "127.0.0.1:0", "public_url": "https://hub.example.invalid",
            "state_file": false, "firmware_dir": false, "config_path": "/etc/peephole.yaml",
            "env": [
                {"name": "PEEPHOLE_CONFIG", "set": true}, {"name": "PEEPHOLE_LISTEN", "set": false},
                {"name": "PEEPHOLE_DEMO", "set": false}, {"name": "PEEPHOLE_STATE", "set": false},
                {"name": "PEEPHOLE_FIRMWARE_DIR", "set": false}, {"name": "PEEPHOLE_PUBLIC_URL", "set": false},
                {"name": "PEEPHOLE_API_TOKEN", "set": true}
            ]
        })
    );
    let p = &v["plugins"][0];
    assert_eq!(p["name"], "p");
    assert_eq!(p["kind"], "prometheus");
    assert_eq!(p["ok"], false);
    assert_eq!(p["error"], "error sending request for url (http://prom.invalid:9090/api)");
    assert_eq!(p["settings"]["url"], json!({"env": "T", "set": true}));
    assert_eq!(p["settings"]["bearer_token"], json!({"env": "T", "set": true, "secret": true}));
    assert_eq!(v["plugins"][1]["settings"]["tz"], json!({"env": "TZ_UNSET", "set": false, "default": "UTC"}));
    assert_eq!(v["pages"], json!(["a", "b"]));
    assert_eq!(v["devices"], json!([]));
}

#[tokio::test]
async fn api_plugins_reports_interval_and_last_poll_without_settings() {
    let state = hub(false);
    state.store.set("d/_ok", Value::Number(1.0));
    state.store.set("d/_error", Value::Text("an old failure".into()));
    state.store.set("d/_values", Value::Number(3.0));
    state.store.set("d/_polled_at", Value::Number(1.0));
    let app = router(state);
    let (status, _, body) = get(&app, "/api/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let v = json_of(&body);
    assert_eq!(
        v[0],
        json!({"name": "clock", "kind": "clock", "interval_s": 1.0, "ok": null, "error": null,
               "polled_at": null, "age_s": null, "values": null})
    );
    let d = &v[1];
    assert_eq!(d["name"], "d");
    assert_eq!(d["interval_s"], 5.0);
    assert_eq!(d["ok"], true);
    assert_eq!(d["error"], Json::Null, "an error from an earlier poll is not shown once a poll succeeds");
    assert_eq!(d["values"], 3);
    assert!(d["polled_at"].is_string());
    assert_eq!(d["age_s"], 0);
    assert!(d.get("settings").is_none());
}

#[tokio::test]
async fn settings_page_is_html_with_the_title_and_links() {
    let app = router(hub(false));
    let (status, headers, body) = get(&app, "/settings").await;
    assert_eq!(status, StatusCode::OK);
    assert!(header(&headers, "content-type").starts_with("text/html"));
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("<title>Settings · Test hub</title>"), "{html}");
    assert!(html.contains(r#"href="/""#) && html.contains(r#"href="/flash/""#));
    assert!(html.contains("/api/settings"));
    assert!(!html.contains(r#"id="token""#), "no token input without PEEPHOLE_API_TOKEN");
    let (_, _, body) = get(&router(hub_with(token_settings(false), CONFIG)), "/settings").await;
    assert!(String::from_utf8(body.to_vec()).unwrap().contains(r#"id="token""#));
    let (_, _, body) = get(&app, "/").await;
    assert!(String::from_utf8(body.to_vec()).unwrap().contains(r#"href="/settings""#));
}
