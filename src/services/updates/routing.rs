use super::preview::{has_callbacks, preview};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    store::{bot_dialog as db, channels, invite_links as link_store},
    telegram::client::TelegramTransport,
};
use serde_json::{Value, json};

pub(super) fn find_message(
    context: &AccountContext,
    message: &Value,
) -> Result<Option<Value>, ServiceError> {
    match (
        message["chat"]["id"].as_i64(),
        message["message_id"].as_i64(),
    ) {
        (Some(chat), Some(mid)) => Ok(db::find_post(context, chat, mid)?),
        _ => Ok(None),
    }
}
pub(super) fn update_chat(
    context: &AccountContext,
    update: &Value,
) -> Result<Option<Value>, ServiceError> {
    if let Some(callback) = update.get("callback_query") {
        let message = &callback["message"];
        if let (Some(chat), Some(mid)) = (
            message["chat"]["id"].as_i64(),
            message["message_id"].as_i64(),
        ) && (db::find_post(context, chat, mid)?.is_some()
            || has_callbacks(&preview(context, chat, mid)?))
        {
            return Ok(Some(json!(chat)));
        }
        return Ok(None);
    }
    if let Some(poll) = update.get("poll") {
        return Ok(
            db::find_poll(context, poll["id"].as_str().unwrap_or(""))?.map(|post| {
                if post["chat_num"].is_null() {
                    post["chat_id"].clone()
                } else {
                    post["chat_num"].clone()
                }
            }),
        );
    }
    if let Some(reaction) = update.get("message_reaction_count") {
        return Ok(find_message(context, reaction)?.map(|_| reaction["chat"]["id"].clone()));
    }
    let event = [
        "message",
        "edited_message",
        "chat_member",
        "chat_join_request",
    ]
    .into_iter()
    .find_map(|key| update.get(key))
    .unwrap_or(&Value::Null);
    if event["is_automatic_forward"].as_bool() == Some(true) {
        let origin = &event["forward_origin"];
        return Ok(find_message(context, origin)?.map(|_| origin["chat"]["id"].clone()));
    }
    if let Some(link) = event["invite_link"]["invite_link"].as_str() {
        return Ok(link_store::get(context, link)?.map(|link| link["chat_id"].clone()));
    }
    let chat = &event["chat"]["id"];
    Ok(
        if !chat.is_null() && channels::get(context, chat)?.is_some() {
            Some(chat.clone())
        } else {
            None
        },
    )
}
pub(super) fn transport(state: &AppState) -> TelegramTransport {
    state.transport()
}
