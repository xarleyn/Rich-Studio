use crate::AppState;
use serde_json::Value;

pub async fn refresh(state: &AppState, force: bool) -> Value {
    let _refresh = state.spec_refresh.lock().await;
    let mut spec = state.spec.read().await.clone();
    let report = spec.refresh(&state.settings, force).await;
    *state.spec.write().await = spec;
    report
}
