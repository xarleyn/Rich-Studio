use super::{
    preview::{has_callbacks, preview},
    routing::{find_message, transport},
};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    services::{blocking, bot_dialog},
    store::{auth, bot_dialog as db},
    telegram::uploads::Uploads,
};
use serde_json::{Value, json};

pub(super) async fn answer(
    state: &AppState,
    context: &AccountContext,
    callback: &Value,
) -> Result<(), ServiceError> {
    let callback = callback.clone();
    let callback_copy = callback.clone();
    let mut params = blocking::run(context, move |context| {
        let message = &callback_copy["message"];
        let data = callback_copy["data"].as_str().unwrap_or("");
        let callbacks = if let Some(post) = find_message(context, message)? {
            db::bump_click(context, post["id"].as_str().unwrap_or(""), data)?;
            post["payload"]["callbacks"].clone()
        } else if let (Some(chat), Some(mid)) = (
            message["chat"]["id"].as_i64(),
            message["message_id"].as_i64(),
        ) {
            preview(context, chat, mid)?
        } else {
            Value::Null
        };
        let answer = &callbacks[data];
        let mut params = json!({});
        if answer.is_string() {
            params["text"] = answer.clone();
        } else if let Some(answer) = answer.as_object() {
            for key in ["text", "show_alert", "url", "cache_time"] {
                if let Some(value) = answer.get(key) {
                    params[key] = value.clone();
                }
            }
        }
        Ok(params)
    })
    .await?;
    params["callback_query_id"] = callback["id"].clone();
    if let Err(error) = transport(state)
        .call(context, "answerCallbackQuery", params, Uploads::default())
        .await
        && !error.to_string().contains("query is too old")
    {
        return Err(error.into());
    }
    Ok(())
}
pub(super) async fn content_callback(
    state: &AppState,
    update: &Value,
) -> Result<bool, ServiceError> {
    if !update["callback_query"].is_object() {
        return Ok(false);
    }
    let Some(uid) = bot_dialog::private_sender(update) else {
        return Ok(false);
    };
    let Some(mid) = update["callback_query"]["message"]["message_id"].as_i64() else {
        return Ok(false);
    };
    let settings = state.settings.clone();
    blocking::run(&AccountContext::root(&settings.clone()), move |_| {
        let context = if settings.public && auth::account(&settings, uid)?.is_some() {
            bot_dialog::context(&settings, uid)
        } else if settings.preview_chat == uid.to_string() {
            AccountContext::root(&settings)
        } else {
            return Ok(false);
        };
        Ok(db::find_post(&context, uid, mid)?.is_some()
            || has_callbacks(&preview(&context, uid, mid)?))
    })
    .await
}
