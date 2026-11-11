use super::{ALLOWED_UPDATES, OFFSET_KEY, dispatch::handle_update, routing::transport};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    services::blocking,
    store::{auth, meta},
    telegram::uploads::Uploads,
};
use serde_json::{Value, json};

async fn processed(state: &AppState, update: &Value) {
    let id = format!("update-{}", update["update_id"]);
    match crate::logs::scope(id, handle_update(state, update)).await {
        Ok(Some(_)) => {
            let mut status = state.updates_status.write().await;
            status.handled = status.handled.saturating_add(1);
            status.last_at = Some(auth::now());
        }
        Err(error) => {
            state.logs.event("ERROR","rich-studio.updates",&error.to_string(),json!({"event":"update.failed","request_id":format!("update-{}",update["update_id"])}));
            state.updates_status.write().await.error =
                Some(crate::logs::scrub(&state.settings, &error.to_string()));
        }
        _ => {}
    }
}
pub async fn receive_webhook(state: &AppState, update: &Value) -> Result<(), ServiceError> {
    let id = update["update_id"]
        .as_i64()
        .filter(|id| *id >= 0)
        .ok_or_else(|| {
            ServiceError::BadRequest(
                "update_id должен быть неотрицательным целым числом".to_owned(),
            )
        })?;
    if blocking::run(&AccountContext::root(&state.settings), move |context| {
        Ok(meta::claim_webhook_update(context, id, auth::now())?)
    })
    .await?
    {
        processed(state, update).await;
    }
    Ok(())
}
pub async fn poll_once(state: &AppState) -> Result<(), ServiceError> {
    let root = AccountContext::root(&state.settings);
    let mut offset = blocking::run(&root, |context| {
        Ok(meta::get(context, OFFSET_KEY)?.and_then(|text| text.parse::<i64>().ok()))
    })
    .await?;
    let updates = transport(state)
        .call(
            &root,
            "getUpdates",
            json!({"offset":offset,"timeout":25,"allowed_updates":ALLOWED_UPDATES}),
            Uploads::default(),
        )
        .await?;
    let updates = updates.as_array().ok_or_else(|| {
        ServiceError::Internal("Telegram вернул неверный список событий".to_owned())
    })?;
    {
        let mut status = state.updates_status.write().await;
        status.state = "ok".to_owned();
        status.error = None;
    }
    for update in updates {
        let Some(id) = update["update_id"]
            .as_i64()
            .filter(|id| *id >= 0 && *id < i64::MAX)
        else {
            continue;
        };
        if offset.is_some_and(|offset| id < offset) {
            continue;
        }
        offset = Some(id + 1);
        blocking::run(&root, move |context| {
            Ok(meta::set(context, OFFSET_KEY, &(id + 1).to_string())?)
        })
        .await?;
        processed(state, update).await;
    }
    Ok(())
}
pub async fn register_webhook(state: &AppState) -> Result<(), ServiceError> {
    transport(state).call(&AccountContext::root(&state.settings),"setWebhook",json!({"url":format!("{}/api/telegram/webhook",state.settings.public_url),"secret_token":state.settings.webhook_secret,"allowed_updates":ALLOWED_UPDATES,"max_connections":1}),Uploads::default()).await?;
    Ok(())
}
