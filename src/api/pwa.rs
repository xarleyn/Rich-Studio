use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::AppState;

pub async fn service_worker(State(state): State<AppState>) -> Response {
    match tokio::fs::read(state.static_dir.join("sw.js")).await {
        Ok(body) => (
            [
                (
                    header::CONTENT_TYPE,
                    "text/javascript; charset=utf-8".to_owned(),
                ),
                (header::CACHE_CONTROL, "no-cache".to_owned()),
                (
                    header::HeaderName::from_static("service-worker-allowed"),
                    "/".to_owned(),
                ),
            ],
            body,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
