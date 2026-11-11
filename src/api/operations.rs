use std::time::Instant;

use axum::{
    Json, Router,
    body::Body,
    extract::{MatchedPath, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt as _;
use serde_json::json;
use tokio_util::io::ReaderStream;

use crate::{AppState, logs, security::ScopedState, services::backup};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/export", get(export))
        .route("/api/logs", get(read_logs))
}

#[utoipa::path(get, path = "/api/export", operation_id = "export_api_export_get", summary = "Скачать бэкап: zip с базой (черновики, посты, история) и загруженными файлами", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn export(ScopedState(state): ScopedState) -> Response {
    let settings = state.settings.clone();
    let account = state.account.clone();
    let export =
        match tokio::task::spawn_blocking(move || backup::export_zip(&settings, &account)).await {
            Ok(Ok(export)) => export,
            _ => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail":"не удалось создать резервную копию"})),
                )
                    .into_response();
            }
        };
    let file = match tokio::fs::File::open(export.path()).await {
        Ok(file) => file,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let size = file.metadata().await.map(|metadata| metadata.len()).ok();
    let stream = ReaderStream::new(file).map(move |chunk| {
        let _keep_until_body_is_dropped = &export;
        chunk
    });
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        backup::export_name()
    )) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    if let Some(size) = size
        && let Ok(value) = HeaderValue::from_str(&size.to_string())
    {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
}

#[utoipa::path(get, path = "/api/logs", operation_id = "read_logs_api_logs_get", summary = "Журнал студии (строки JSON), новые в конце; без токена и текста постов", description = "`q=<request_id>` — всё, что сделал один запрос.", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn read_logs(
    ScopedState(state): ScopedState,
    Query(query): Query<logs::ReadQuery>,
) -> Response {
    match logs::read_checked(state.logs.clone(),query,1000).await{
        Ok(lines)=>Json(json!({"file":state.logs.current_file().map(|path|path.to_string_lossy().into_owned()).unwrap_or_default(),"lines":lines})).into_response(),
        Err(message)=>(StatusCode::UNPROCESSABLE_ENTITY,Json(json!({"detail":message}))).into_response(),
    }
}

/// Installed outside the access guard so rejected requests receive correlation IDs as well.
pub async fn observe(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let (id, marketing, actor) = {
        let incoming = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
        };
        (
            logs::incoming_id(incoming("x-request-id"), incoming("traceparent")),
            state
                .settings
                .is_landing_host(incoming("host").unwrap_or_default()),
            incoming("x-studio-client")
                .unwrap_or("api")
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect::<String>(),
        )
    };
    let raw_path = request.uri().path().to_owned();
    let path = raw_path
        .chars()
        .take(200)
        .map(|character| {
            if character.is_control() {
                format!("\\x{:02x}", character as u32)
            } else {
                character.to_string()
            }
        })
        .collect::<String>();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("other", MatchedPath::as_str)
        .to_owned();
    let method = request.method().as_str().to_owned();
    let started = Instant::now();
    logs::scope(id.clone(),async move{
        let mut response=next.run(request).await;
        let status=response.status().as_u16();let ms=started.elapsed().as_millis().min(u64::MAX as u128)as u64;
        if !raw_path.starts_with("/static/")&&!raw_path.starts_with("/media/"){state.metrics.http_observe(&method,&route,status,ms);}
        // Docker and agents poll these; a line per call would bury everything else.
        if !raw_path.starts_with("/static/")&&!raw_path.starts_with("/media/")&&!["/healthz","/readyz","/api/health","/api/logs","/favicon.ico"].contains(&raw_path.as_str()) {
            state.logs.event(if status>=500{"WARNING"}else{"INFO"},"rich-studio.http",&format!("{method} {path} {status}"),json!({"event":"http","method":method,"path":path,"route":route,"status":status,"ms":ms,"client":actor}));
        }
        if let Ok(value)=HeaderValue::from_str(&id){response.headers_mut().insert("x-request-id",value);}
        if !marketing||crate::public_site::PREVIEW_PATHS.contains(&path.as_str()){response.headers_mut().insert("x-robots-tag",HeaderValue::from_static("noindex, nofollow"));}
        response
    }).await
}
