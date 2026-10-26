mod api;
mod devices;
mod flash;
mod frame;
mod plugins;

use axum::extract::{FromRequest, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Every route of the hub.
pub fn router(state: AppState) -> Router {
    let firmware = flash::firmware_service(&state);
    Router::new()
        .route("/", get(api::index))
        .route("/healthz", get(api::healthz))
        .route("/frame.png", get(frame::device_frame))
        .route("/pages/{file}", get(frame::page_frame))
        .route("/api/pages", get(api::pages))
        .route("/api/devices", get(api::devices))
        .route("/api/devices/{id}", get(api::device))
        .route("/api/values", get(api::values))
        .route("/api/config", get(api::config))
        .route("/api/devices/{id}/next", post(devices::next))
        .route("/api/devices/{id}/prev", post(devices::prev))
        .route("/api/devices/{id}/page", post(devices::page))
        .route("/api/devices/{id}/message", post(devices::message))
        .route("/api/devices/{id}/name", post(devices::name))
        .route("/api/devices/{id}/cycle", post(devices::cycle))
        .route("/api/devices/{id}/pages", post(devices::pages))
        .route("/api/plugins/{name}/poll", post(plugins::poll))
        .route("/flash", get(flash::redirect))
        .route("/flash/", get(flash::page))
        .route("/flash/manifest.json", get(flash::manifest))
        .merge(firmware)
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), guard_posts))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// A JSON error: `{ "error": "..." }` with a status.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    pub fn unknown_device(id: &str) -> Self {
        Self::not_found(format!("device {id:?} is not known"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}

/// `Json<T>` whose rejections are JSON errors too.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError::bad_request(rejection.body_text())),
        }
    }
}

/// Every `POST` is refused in demo mode (403) and, when `PEEPHOLE_API_TOKEN` is set, needs its bearer token (401).
async fn guard_posts(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if req.method() != Method::POST {
        return next.run(req).await;
    }
    if state.demo {
        return ApiError::new(StatusCode::FORBIDDEN, "this hub runs in demo mode: changes are disabled")
            .into_response();
    }
    if let Some(token) = &state.api_token {
        let presented =
            req.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        if !presented.is_some_and(|p| token.matches(p)) {
            return ApiError::new(
                StatusCode::UNAUTHORIZED,
                "this hub needs Authorization: Bearer <PEEPHOLE_API_TOKEN>",
            )
            .into_response();
        }
    }
    next.run(req).await
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}
