use crate::security::ScopedState;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use crate::AppState;

async fn inspect(state: AppState) -> Result<Value, String> {
    let spec = state.spec.read().await.clone();
    let loops = state.loop_status.read().await.clone();
    let updates = state.updates_status.read().await.clone();
    tokio::task::spawn_blocking(move || {
        crate::services::health::report(
            &state.settings,
            &state.account,
            &spec,
            &state.logs,
            &state.metrics,
            &loops,
            &updates,
        )
    })
    .await
    .map_err(|_| "health worker failed".to_owned())
}

#[utoipa::path(get, path = "/api/health", operation_id = "health_report_api_health_get", summary = "Состояние студии: проверки ok/warn/fail с подсказкой, что делать, счётчики и последние ошибки", tag = "service", responses((status = 200, description = "Successful Response")))]
pub async fn report(ScopedState(state): ScopedState) -> Response {
    match inspect(state).await {
        Ok(report) => Json(report).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail":"проверка состояния не выполнена"})),
        )
            .into_response(),
    }
}

pub async fn ready(ScopedState(state): ScopedState) -> Response {
    let mut cache = state.ready_cache.value.lock().await;
    let cached = cache
        .as_ref()
        .filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(30))
        .map(|(_, report)| report.clone());
    let report = match cached {
        Some(report) => Ok(report),
        None => {
            let report = inspect(state.clone()).await;
            if let Ok(report) = &report {
                *cache = Some((std::time::Instant::now(), report.clone()));
            }
            report
        }
    };
    let failing: Vec<Value> = match report {
        Ok(report) => report["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|check| check["status"] == "fail")
            .map(|check| check["name"].clone())
            .collect(),
        Err(_) => vec![json!("health")],
    };
    (
        if failing.is_empty() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({"ready":failing.is_empty(),"failing":failing})),
    )
        .into_response()
}
