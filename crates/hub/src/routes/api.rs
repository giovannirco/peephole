use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::Html;
use chrono::Utc;
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
