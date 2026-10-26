use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::Html;
use chrono::Utc;
use peephole_core::config::{redact_text, redact_url};
use peephole_core::{Device, Value};
use serde::Serialize;
use serde_json::{Value as Json_, json};

use super::ApiError;
use crate::state::AppState;
use crate::ui;

pub async fn index(State(state): State<AppState>) -> Html<String> {
    Html(ui::index(&state))
}

pub async fn healthz(State(state): State<AppState>) -> Json<Json_> {
    Json(json!({
        "ok": true,
        "plugins": state.cfg.plugins.len(),
        "devices": state.registry.list().len(),
        "pages": state.cfg.pages.len(),
    }))
}

#[derive(Serialize)]
pub struct PageSummary {
    name: String,
    title: Option<String>,
    width: u32,
    height: u32,
    cards: usize,
}

pub async fn pages(State(state): State<AppState>) -> Json<Vec<PageSummary>> {
    Json(
        state
            .cfg
            .pages
            .iter()
            .map(|p| PageSummary {
                name: p.name.clone(),
                title: p.title.clone(),
                width: p.width,
                height: p.height,
                cards: p.cards.len(),
            })
            .collect(),
    )
}

pub async fn devices(State(state): State<AppState>) -> Json<Vec<Device>> {
    Json(state.registry.list())
}

pub async fn device(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<Device>, ApiError> {
    state.registry.get(&id).map(Json).ok_or_else(|| ApiError::unknown_device(&id))
}

pub async fn values(State(state): State<AppState>) -> Json<BTreeMap<String, Json_>> {
    let now = Utc::now();
    Json(
        state
            .store
            .snapshot()
            .into_iter()
            .map(|(key, stamped)| {
                let entry = json!({
                    "value": summarise(&stamped.value),
                    "updated_at": stamped.updated_at,
                    "age_s": stamped.age_secs(now),
                });
                (key, entry)
            })
            .collect(),
    )
}

fn summarise(value: &Value) -> Json_ {
    match value {
        Value::Number(n) => json!(n),
        Value::Text(t) => json!(t),
        Value::Series(s) => json!(s),
        Value::Image { width, height, .. } => json!({ "image": [width, height] }),
    }
}

pub async fn config(State(state): State<AppState>) -> Json<Json_> {
    let plugins: Vec<Json_> = state.cfg.plugins.iter().map(|p| json!({ "name": p.name(), "kind": p.kind })).collect();
    Json(json!({
        "title": state.cfg.hub.title,
        "cycle_s": state.cfg.hub.cycle_s,
        "stale_s": state.cfg.hub.stale_s,
        "demo": state.demo,
        "auth": state.api_token.is_some(),
        "plugins": plugins,
    }))
}

/// `GET /api/plugins`: each plugin's interval and last poll.
pub async fn plugins(State(state): State<AppState>) -> Json<Vec<Json_>> {
    Json(plugin_statuses(&state))
}

fn plugin_statuses(state: &AppState) -> Vec<Json_> {
    let now = Utc::now();
    state
        .cfg
        .plugins
        .iter()
        .map(|p| {
            let name = p.name();
            let ok = state.store.num(&format!("{name}/_ok")).map(|n| n == 1.0);
            let error = state.store.text(&format!("{name}/_error")).filter(|_| ok == Some(false));
            let polled = state.store.get(&format!("{name}/_polled_at"));
            json!({
                "name": name,
                "kind": p.kind,
                "interval_s": state.intervals.get(name).map(|d| d.as_secs_f64()),
                "ok": ok,
                "error": error.map(|e| redact_text(&e)),
                "polled_at": polled.as_ref().map(|s| s.updated_at),
                "age_s": polled.as_ref().map(|s| s.age_secs(now)),
                "values": state.store.num(&format!("{name}/_values")).map(|n| n as u64),
            })
        })
        .collect()
}

/// `GET /api/settings`: how the hub is configured, safe to show (no secret, no expanded variable).
pub async fn settings(State(state): State<AppState>) -> Json<Json_> {
    let env: Vec<Json_> = state.env.iter().map(|(name, set)| json!({ "name": name, "set": set })).collect();
    let plugins: Vec<Json_> = plugin_statuses(&state)
        .into_iter()
        .map(|mut p| {
            let shown = state.cfg.redacted.iter().find(|r| p["name"] == r.name.as_str());
            p["settings"] = shown.map_or(Json_::Null, |r| r.settings.clone());
            p
        })
        .collect();
    Json(json!({
        "hub": {
            "title": state.cfg.hub.title,
            "cycle_s": state.cfg.hub.cycle_s,
            "stale_s": state.cfg.hub.stale_s,
            "demo": state.demo,
            "auth": state.api_token.is_some(),
            "listen": state.listen.to_string(),
            "public_url": state.public_url.as_deref().map(redact_url),
            "state_file": state.state_file,
            "firmware_dir": state.firmware_dir.is_some(),
            "config_path": state.config_path.as_ref().map(|p| p.display().to_string()),
            "env": env,
        },
        "plugins": plugins,
        "devices": state.registry.list(),
        "pages": state.page_names(),
    }))
}

pub async fn settings_page(State(state): State<AppState>) -> Html<String> {
    Html(ui::settings(&state))
}
