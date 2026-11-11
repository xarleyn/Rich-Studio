use super::{
    COMMENT_WAIT,
    ingest::{poll_once, register_webhook},
};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    services::{accounts, blocking, bot_dialog, bot_notifications},
    store::auth,
    telegram::TelegramError,
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

async fn expire(state: &AppState) -> Result<(), ServiceError> {
    let settings = state.settings.clone();
    let contexts = blocking::run(&AccountContext::root(&settings.clone()), move |_| {
        accounts::all_contexts(&settings).map_err(|e| ServiceError::Internal(e.to_string()))
    })
    .await?;
    for context in contexts {
        bot_notifications::expire_comments(
            &context,
            &state.settings,
            auth::now() - COMMENT_WAIT,
            "первый комментарий: пересылка в обсуждение не пришла за сутки",
        )
        .await?;
    }
    Ok(())
}
async fn pause(stopping: &AtomicBool, seconds: u64) {
    for _ in 0..seconds {
        if stopping.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
async fn incoming(state: &AppState, stopping: &AtomicBool) {
    let mut registered = false;
    while !stopping.load(Ordering::Acquire) {
        let status = state.updates_status.read().await.clone();
        state.metrics.beat("updates", 60.0, status.error.as_deref());
        if !state.settings.updates_on {
            pause(stopping, 1).await;
            continue;
        }
        if state.settings.bot_token.is_empty() {
            state.updates_status.write().await.state = "no_token".to_owned();
            pause(stopping, 30).await;
            continue;
        }
        let result = if state.settings.updates_mode == "webhook" {
            if !registered {
                match register_webhook(state).await {
                    Ok(()) => {
                        registered = true;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            } else {
                Ok(())
            }
        } else {
            poll_once(state).await
        };
        let result = match result {
            Ok(()) => expire(state).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                let mut status = state.updates_status.write().await;
                status.state = "ok".to_owned();
                status.error = None;
                drop(status);
                if state.settings.updates_mode == "webhook" {
                    pause(stopping, 60).await;
                }
            }
            Err(error) => {
                let conflict = matches!(
                    error,
                    ServiceError::Telegram(TelegramError::Refused {
                        code: Some(409),
                        ..
                    })
                );
                let mut status = state.updates_status.write().await;
                status.state = if conflict { "conflict" } else { "error" }.to_owned();
                status.error = Some(crate::logs::scrub(&state.settings, &error.to_string()));
                drop(status);
                pause(stopping, if conflict { 60 } else { 10 }).await;
            }
        }
    }
}
async fn reports(state: &AppState, stopping: &AtomicBool) {
    let mut registered = false;
    while !stopping.load(Ordering::Acquire) {
        let mut error = None;
        if !state.settings.bot_token.is_empty() {
            if !registered {
                match bot_dialog::register_commands(state).await {
                    Ok(()) => registered = true,
                    Err(failure) => {
                        state.logs.event(
                            "ERROR",
                            "rich-studio.bot",
                            &failure.to_string(),
                            json!({"event":"bot.menu_failed"}),
                        );
                        error = Some(crate::logs::scrub(&state.settings, &failure.to_string()));
                    }
                }
            }
            if let Err(failure) = bot_notifications::deliver(state).await {
                state.logs.event(
                    "ERROR",
                    "rich-studio.bot",
                    &failure.to_string(),
                    json!({"event":"bot.delivery_failed"}),
                );
                error = Some(crate::logs::scrub(&state.settings, &failure.to_string()));
            }
        }
        state
            .metrics
            .beat("bot_notifications", 10.0, error.as_deref());
        pause(stopping, 10).await;
    }
}
/// Child futures stay owned by the lease worker; aborting it cannot orphan any sender.
pub async fn run(state: AppState, stopping: Arc<AtomicBool>) -> Result<(), ServiceError> {
    tokio::join!(incoming(&state, &stopping), reports(&state, &stopping));
    Ok(())
}
