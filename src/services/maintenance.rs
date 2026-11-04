//! Leader-owned maintenance; all SQLite and filesystem work stays off the async executor.
use super::{accounts, backup, health, housekeeping, loops::pause};
use crate::{AppState, errors::ServiceError};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub async fn run(state: &AppState, stop: Arc<AtomicBool>) {
    tokio::join!(
        spec_loop(state, stop.clone()),
        housekeeping_loop(state, stop.clone()),
        backup_loop(state, stop)
    );
}
fn report(state: &AppState, name: &str, interval: f64, error: Option<&str>) {
    state.metrics.beat(name, interval, error);
    if let Some(error) = error {
        state.logs.event(
            "ERROR",
            &format!("rich-studio.{name}"),
            error,
            json!({"event":format!("{name}.error")}),
        );
    }
}
async fn spec_loop(state: &AppState, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        // A slow network refresh must not hold the readers of the editor's specification.
        let result = super::specs::refresh(state, false).await;
        let error = if state.settings.spec_pinned {
            None
        } else {
            result["error"].as_str()
        };
        report(state, "spec", 1800.0, error);
        pause(&stop, 1800).await;
    }
}
async fn housekeeping_loop(state: &AppState, stop: Arc<AtomicBool>) {
    let mut previous = BTreeMap::new();
    let mut cleanup_at = 0_i64;
    while !stop.load(Ordering::Acquire) {
        let spec = state.spec.read().await.clone();
        let loops = state.loop_status.read().await.clone();
        let copy = state.clone();
        let now = housekeeping::now();
        let cleanup = now.saturating_sub(cleanup_at) >= 3600;
        let outcome = tokio::task::spawn_blocking(move || {
            if cleanup {
                let contexts =
                    accounts::all_contexts(&copy.settings).map_err(|error| error.to_string())?;
                let mut errors = Vec::new();
                for context in contexts {
                    if let Err(error) = housekeeping::cleanup(&copy.settings, &context, now) {
                        errors.push(error.to_string());
                    }
                }
                if !errors.is_empty() {
                    return Err(errors.join("; "));
                }
            }
            Ok::<Value, String>(health::report(
                &copy.settings,
                &copy.account,
                &spec,
                &copy.logs,
                &copy.metrics,
                &loops,
            ))
        })
        .await;
        let result = outcome
            .map_err(|_| "maintenance worker interrupted".to_owned())
            .and_then(|result| result);
        let (status, error) = match result {
            Ok(value) => {
                if cleanup {
                    cleanup_at = now;
                }
                housekeeping::watch(&value, &mut previous, &state.logs);
                (value["status"].as_str().unwrap_or("fail").to_owned(), None)
            }
            Err(error) => ("fail".to_owned(), Some(error)),
        };
        housekeeping::ping(&state.settings, &status, &state.logs).await;
        report(state, "housekeeping", 300.0, error.as_deref());
        pause(&stop, 300).await;
    }
}
async fn backup_loop(state: &AppState, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) && state.settings.backup_keep > 0 {
        let copy = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            let contexts = accounts::all_contexts(&copy.settings)
                .map_err(|error| ServiceError::Internal(error.to_string()))?;
            let mut errors = Vec::new();
            for context in contexts {
                if let Err(error) = backup::daily_snapshot(&copy.settings, &context) {
                    errors.push(error.to_string());
                }
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(ServiceError::Internal(errors.join("; ")))
            }
        })
        .await;
        let error = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("backup worker interrupted".to_owned()),
        };
        report(state, "backup", 3600.0, error.as_deref());
        pause(&stop, 3600).await;
    }
}
