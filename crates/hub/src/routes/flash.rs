use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::handler::HandlerWithoutStateExt;
use axum::http::StatusCode;
use axum::response::{Html, Redirect};
use axum::routing::get;
use serde_json::Value as Json_;
use tower_http::services::ServeDir;

use super::ApiError;
use crate::firmware;
use crate::state::AppState;
use crate::ui;

pub async fn redirect() -> Redirect {
    Redirect::permanent("/flash/")
}

pub async fn page(State(state): State<AppState>) -> Html<String> {
    let builds = available(&state).unwrap_or_default();
    Html(ui::flash(&state, &builds))
}

pub async fn manifest(State(state): State<AppState>) -> Result<Json<Json_>, ApiError> {
    let builds = available(&state)?;
    firmware::manifest(&builds).map(Json).ok_or_else(|| ApiError::not_found("no firmware files are present"))
}

fn available(state: &AppState) -> Result<Vec<firmware::Firmware>, ApiError> {
    let dir = state.firmware_dir.as_ref().ok_or_else(firmware_unset_error)?;
    firmware::scan(dir).map_err(|e| {
        tracing::error!(error = %e, dir = %dir.display(), "reading the firmware dir failed");
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "the firmware dir could not be read")
    })
}

fn firmware_unset_error() -> ApiError {
    ApiError::not_found("no firmware dir is configured (PEEPHOLE_FIRMWARE_DIR)")
}

async fn firmware_unset() -> ApiError {
    firmware_unset_error()
}

async fn firmware_missing() -> ApiError {
    ApiError::not_found("no such firmware file")
}

/// `/firmware/*` from `PEEPHOLE_FIRMWARE_DIR`, or JSON 404s when it is unset.
pub fn firmware_service(state: &AppState) -> Router<AppState> {
    match &state.firmware_dir {
        Some(dir) => Router::new()
            .nest_service("/firmware", ServeDir::new(dir).not_found_service(firmware_missing.into_service())),
        None => Router::new().route("/firmware/{*file}", get(firmware_unset)),
    }
}
