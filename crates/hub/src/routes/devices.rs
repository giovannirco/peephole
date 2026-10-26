use axum::Json;
use axum::extract::{Path, State};
use chrono::Utc;
use peephole_core::{Device, PageError};
use serde::Deserialize;

use super::{ApiError, JsonBody};
use crate::state::AppState;

const MAX_MESSAGE_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 64;
const MAX_CYCLE_S: u64 = 86_400;

type DeviceResult = Result<Json<Device>, ApiError>;

fn found(id: &str, device: Option<Device>) -> DeviceResult {
    device.map(Json).ok_or_else(|| ApiError::unknown_device(id))
}

fn step(state: &AppState, id: &str, delta: i64) -> DeviceResult {
    found(id, state.registry.step(id, delta, &state.page_names(), Utc::now()))
}

pub async fn next(State(state): State<AppState>, Path(id): Path<String>) -> DeviceResult {
    step(&state, &id, 1)
}

pub async fn prev(State(state): State<AppState>, Path(id): Path<String>) -> DeviceResult {
    step(&state, &id, -1)
}

#[derive(Deserialize)]
pub struct PageBody {
    name: String,
}

pub async fn page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<PageBody>,
) -> DeviceResult {
    match state.registry.set_page(&id, &body.name, &state.page_names(), Utc::now()) {
        Ok(device) => found(&id, device),
        Err(PageError::Unknown(_)) => Err(ApiError::not_found(format!("page {:?} is not configured", body.name))),
        Err(e) => Err(ApiError::bad_request(e.to_string())),
    }
}

#[derive(Deserialize)]
pub struct PagesBody {
    pages: Vec<String>,
}

pub async fn pages(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<PagesBody>,
) -> DeviceResult {
    let device = state
        .registry
        .set_pages(&id, body.pages, &state.page_names(), Utc::now())
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    found(&id, device)
}

#[derive(Deserialize)]
pub struct MessageBody {
    text: String,
}

pub async fn message(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<MessageBody>,
) -> DeviceResult {
    if body.text.chars().count() > MAX_MESSAGE_CHARS {
        return Err(ApiError::bad_request(format!("text is longer than {MAX_MESSAGE_CHARS} characters")));
    }
    found(&id, state.registry.set_message(&id, &body.text))
}

#[derive(Deserialize)]
pub struct NameBody {
    name: String,
}

pub async fn name(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<NameBody>,
) -> DeviceResult {
    let name = body.name.trim();
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::bad_request(format!("name is longer than {MAX_NAME_CHARS} characters")));
    }
    let name = (!name.is_empty()).then(|| name.to_owned());
    found(&id, state.registry.update(&id, |d| d.name = name))
}

#[derive(Deserialize)]
pub struct CycleBody {
    cycle_s: u64,
}

pub async fn cycle(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<CycleBody>,
) -> DeviceResult {
    if body.cycle_s > MAX_CYCLE_S {
        return Err(ApiError::bad_request(format!("cycle_s must be at most {MAX_CYCLE_S}")));
    }
    let now = Utc::now();
    found(
        &id,
        state.registry.update(&id, |d| {
            d.cycle_s = body.cycle_s;
            d.page_since = now;
        }),
    )
}
