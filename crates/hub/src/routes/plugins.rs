use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value as Json_, json};

use super::ApiError;
use crate::state::AppState;

/// `POST /api/plugins/{name}/poll`: poll a plugin now instead of at its next tick.
pub async fn poll(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<Json_>), ApiError> {
    if !state.polls.poll_now(&name) {
        return Err(ApiError::not_found(format!("plugin {name:?} is not configured")));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "polled": name }))))
}
