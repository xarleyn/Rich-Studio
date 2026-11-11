use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    services::{
        accounts, blocking, idempotency::MAY_HAVE_GONE, in_flight, kinds, publisher::Publisher,
    },
    store::{posts, scheduler},
    telegram::TelegramError,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub fn now() -> i64 {
    crate::store::auth::now()
}

pub async fn run_all(state: &AppState, stopping: Arc<AtomicBool>) -> Result<usize, ServiceError> {
    let settings = state.settings.clone();
    let contexts = tokio::task::spawn_blocking(move || accounts::all_contexts(&settings))
        .await
        .map_err(|_| ServiceError::Internal("account worker interrupted".to_owned()))?
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let mut count = 0;
    let mut errors = Vec::new();
    for context in contexts {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        match run_due(state, context, now(), stopping.clone()).await {
            Ok(done) => count += done,
            Err(error) => errors.push(error.to_string()),
        }
    }
    if errors.is_empty() {
        Ok(count)
    } else {
        Err(ServiceError::Internal(errors.join("; ")))
    }
}

pub async fn run_due(
    state: &AppState,
    context: AccountContext,
    at: i64,
    stopping: Arc<AtomicBool>,
) -> Result<usize, ServiceError> {
    let due = blocking::run(&context, move |context| Ok(scheduler::due(context, at)?)).await?;
    let publisher = Publisher::new(
        context.clone(),
        state.transport(),
        state.settings.updates_on,
    )
    .with_settings(&state.settings);
    let mut count = 0;
    for queued in due {
        if stopping.load(Ordering::Acquire) {
            break;
        }
        let id = queued["id"]
            .as_str()
            .ok_or_else(|| ServiceError::Internal("post id missing".to_owned()))?
            .to_owned();
        let notification_id = id.clone();
        let claim_id = id.clone();
        let stop = stopping.clone();
        let claimed = blocking::run(&context, move |context| {
            if stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            in_flight::claim(context, &claim_id, at)
        })
        .await?;
        let Some(_flight) = claimed else {
            continue;
        };
        let read_id = id.clone();
        let record = blocking::run(&context, move |context| Ok(posts::get(context, &read_id)?))
            .await?
            .unwrap_or(queued);
        count += 1;
        let late = at - record["scheduled_at"].as_i64().unwrap_or(at);
        if state.settings.schedule_late_min > 0.0
            && late as f64 > state.settings.schedule_late_min * 60.0
        {
            let minutes = late / 60;
            let text = if minutes >= 60 {
                format!("{} ч {} мин", minutes / 60, minutes % 60)
            } else {
                format!("{minutes} мин")
            };
            blocking::run(&context, move |context| Ok(scheduler::missed(context,&id,&format!("студия не работала в назначенное время (опоздание {text}): пост не отправлен, опубликуйте или отложите заново"))?)).await?;
            publisher.notify(&notification_id, None, None).await;
            continue;
        }
        let kind = match kinds::get(record["kind"].as_str()) {
            Ok(kind) => kind,
            Err(error) => {
                let detail = error.to_string();
                blocking::run(&context, move |context| {
                    Ok(posts::mark_failed(context, &id, &detail)?)
                })
                .await?;
                publisher.notify(&notification_id, None, None).await;
                continue;
            }
        };
        let chat = match &record["chat_id"] {
            Value::String(text) if !text.trim().is_empty() => text
                .parse::<i64>()
                .map_or_else(|_| json!(text), |id| json!(id)),
            Value::String(_) | Value::Null => {
                // A row can outlive the channel setting it was created with (an old
                // database or a hand-made queue entry): resolve the default at send time
                // and refuse before Telegram when there is none.
                let fallback = state.settings.default_chat.trim();
                if fallback.is_empty() {
                    let detail = "не указан chat_id и не задан TG_DEFAULT_CHAT".to_owned();
                    blocking::run(&context, move |context| {
                        Ok(posts::mark_failed(context, &id, &detail)?)
                    })
                    .await?;
                    publisher.notify(&notification_id, None, None).await;
                    continue;
                }
                fallback
                    .parse::<i64>()
                    .map_or_else(|_| json!(fallback), |id| json!(id))
            }
            value => value.clone(),
        };
        let result = match publisher.send(&chat, &record["payload"], kind).await {
            Ok(result) => result,
            Err(error) => {
                let retry = match &error {
                    TelegramError::Unreachable(_) => Some(60),
                    TelegramError::RateLimited {
                        retry_after: Some(wait),
                        ..
                    } if *wait > 0 => i64::try_from(*wait).ok(),
                    _ => None,
                };
                let (detail, event) = if matches!(error, TelegramError::NoAnswer(_)) {
                    (format!("{error}. {MAY_HAVE_GONE}"), Some("uncertain"))
                } else {
                    (error.to_string(), retry.map(|_| "retry"))
                };
                blocking::run(&context, move |context| {
                    if let Some(wait) = retry {
                        scheduler::retry(
                            context,
                            &id,
                            at.saturating_add(wait),
                            &format!("{detail}; повтор через {wait} с"),
                        )?;
                    } else {
                        posts::mark_failed(context, &id, &detail)?;
                    }
                    Ok(())
                })
                .await?;
                publisher.notify(&notification_id, event, None).await;
                continue;
            }
        };
        let Some(message_id) = result["message_id"].as_i64() else {
            blocking::run(&context, move |context| {
                Ok(posts::mark_failed(context, &id, MAY_HAVE_GONE)?)
            })
            .await?;
            publisher
                .notify(&notification_id, Some("uncertain"), None)
                .await;
            continue;
        };
        let sent = blocking::run(&context, move |context| {
            Ok(posts::mark_sent(context, &id, message_id)?)
        })
        .await?;
        // Failures after confirmation may add a warning, never restore the send to the queue.
        let warnings = publisher
            .after_send(
                &sent,
                &record["payload"],
                &result,
                record["actor"].as_str().unwrap_or("api"),
                kind,
            )
            .await;
        let warnings = match warnings {
            Ok(items) => items,
            Err(error) => vec![error.to_string()],
        };
        publisher
            .notify(&notification_id, None, Some(&warnings.join("; ")))
            .await;
        if !warnings.is_empty() {
            let id = sent["id"].as_str().unwrap_or_default().to_owned();
            blocking::run(&context, move |context| {
                crate::store::db::Store::connect(context)?
                    .execute(
                        "update posts set error=? where id=?",
                        rusqlite::params![warnings.join("; "), id],
                    )
                    .map_err(|error| crate::errors::StoreError::from_sqlite(error, context))?;
                Ok(())
            })
            .await?;
        }
    }
    Ok(count)
}
