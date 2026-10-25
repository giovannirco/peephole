use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use peephole_render::{Frame, RenderCtx, render_notice, render_page};

use super::ApiError;
use crate::state::AppState;

const DEFAULT_WIDTH: u32 = 320;
const DEFAULT_HEIGHT: u32 = 170;
const MAX_SIDE: u32 = 2048;
const MAX_ID_LEN: usize = 64;
const PAGE_HEADER: HeaderName = HeaderName::from_static("x-peephole-page");
const NOTICE_PAGE: &str = "notice";

/// `GET /frame.png?id=&w=&h=`: what a board shows now.
pub async fn device_frame(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let id = query.get("id").map(|s| s.trim()).filter(|s| !s.is_empty());
    let Some(id) = id else { return Err(ApiError::bad_request("id is required")) };
    if id.chars().count() > MAX_ID_LEN {
        return Err(ApiError::bad_request(format!("id is longer than {MAX_ID_LEN} characters")));
    }
    let (width, height) = dimensions(&query)?;
    let now = Utc::now();
    let device = state.registry.touch(id, width, height, now);
    let index = state.registry.current_page(id, &state.page_names(), now).filter(|i| *i < state.cfg.pages.len());
    Ok(png(state, index, device.message, width, height).await)
}

/// `GET /pages/{name}.png?w=&h=`: a page for the browser, no device involved.
pub async fn page_frame(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let name = file.strip_suffix(".png").ok_or_else(|| ApiError::not_found("page images end in .png"))?;
    let index =
        state.page_index(name).ok_or_else(|| ApiError::not_found(format!("page {name:?} is not configured")))?;
    let (width, height) = dimensions(&query)?;
    Ok(png(state, Some(index), String::new(), width, height).await)
}

fn dimensions(query: &HashMap<String, String>) -> Result<(u32, u32), ApiError> {
    Ok((side(query, "w", DEFAULT_WIDTH)?, side(query, "h", DEFAULT_HEIGHT)?))
}

fn side(query: &HashMap<String, String>, key: &str, default: u32) -> Result<u32, ApiError> {
    let Some(text) = query.get(key) else { return Ok(default) };
    match text.parse::<u32>() {
        Ok(v) if (1..=MAX_SIDE).contains(&v) => Ok(v),
        _ => Err(ApiError::bad_request(format!("{key} must be a whole number from 1 to {MAX_SIDE}"))),
    }
}

async fn png(state: AppState, index: Option<usize>, message: String, width: u32, height: u32) -> Response {
    let page_name = index.and_then(|i| state.page(i)).map_or(NOTICE_PAGE.to_owned(), |p| p.name.clone());
    let bytes = tokio::task::spawn_blocking(move || encode(draw(&state, index, &message, width, height)))
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "frame task failed");
            encode(Frame::blank(width, height))
        });
    let mut resp = ([(CONTENT_TYPE, "image/png"), (CACHE_CONTROL, "no-store")], bytes).into_response();
    if let Ok(value) = HeaderValue::from_bytes(page_name.as_bytes()) {
        resp.headers_mut().insert(PAGE_HEADER, value);
    }
    resp
}

fn draw(state: &AppState, index: Option<usize>, message: &str, width: u32, height: u32) -> Frame {
    let Some((index, page)) = index.and_then(|i| state.page(i).map(|p| (i, p))) else {
        return notice(&state.cfg.hub.title, "No pages are configured", width, height);
    };
    let ctx = RenderCtx {
        now: Utc::now(),
        stale_secs: state.cfg.hub.stale_s,
        page_index: index,
        page_count: state.cfg.pages.len(),
        message: message.to_owned(),
        sources: state.sources(),
    };
    catch_unwind(AssertUnwindSafe(|| render_page(page, &state.store, &ctx, width, height))).unwrap_or_else(|_| {
        tracing::error!(page = %page.name, "rendering the page panicked");
        notice("Render failed", &page.name, width, height)
    })
}

fn notice(title: &str, body: &str, width: u32, height: u32) -> Frame {
    catch_unwind(AssertUnwindSafe(|| render_notice(title, body, width, height))).unwrap_or_else(|_| {
        tracing::error!(title, "rendering a notice panicked");
        Frame::blank(width, height)
    })
}

fn encode(frame: Frame) -> Vec<u8> {
    let (width, height) = (frame.width, frame.height);
    frame.to_png().unwrap_or_else(|e| {
        tracing::error!(error = %e, "encoding the frame failed");
        Frame::blank(width, height).to_png().expect("a blank frame always encodes")
    })
}
