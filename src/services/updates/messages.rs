use super::routing::{find_message, transport};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    services::{blocking, bot_notifications, channels::ChannelService},
    store::{bot_dialog as db, posts},
    telegram::TelegramError,
};
use serde_json::Value;

pub(super) async fn connect(
    state: &AppState,
    update: &Value,
    contexts: &[AccountContext],
) -> Result<Option<&'static str>, ServiceError> {
    let member = &update["my_chat_member"];
    let uid = member["from"]["id"].as_i64();
    if let Some(context) = contexts
        .iter()
        .find(|context| context.account_id.is_some() && context.account_id == uid)
        && ["administrator", "creator"]
            .contains(&member["new_chat_member"]["status"].as_str().unwrap_or(""))
        && !member["chat"]["id"].is_null()
    {
        ChannelService::new(context.clone(), transport(state), String::new())
            .check(member["chat"]["id"].clone())
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        return Ok(Some("channel_connected"));
    }
    Ok(None)
}
pub(super) async fn comment(
    state: &AppState,
    context: &AccountContext,
    message: &Value,
) -> Result<Option<&'static str>, ServiceError> {
    let origin = &message["forward_origin"];
    if origin["type"] != "channel" {
        return Ok(None);
    }
    let origin_copy = origin.clone();
    let post = blocking::run(context, move |context| {
        let Some(post) = find_message(context, &origin_copy)? else {
            return Ok(None);
        };
        if post["comment_status"] != "waiting"
            || !db::claim_comment(context, post["id"].as_str().unwrap_or(""))?
        {
            return Ok(None);
        }
        Ok(Some(post))
    })
    .await?;
    let Some(post) = post else { return Ok(None) };
    let result = transport(state)
        .send_linked_comment(
            context,
            &origin["chat"]["id"],
            &message["chat"]["id"],
            message["message_id"].as_i64().unwrap_or(0),
            post["payload"]["comment"].clone(),
        )
        .await;
    let (mid,error)=match result {
        Ok(result) if result["message_id"].as_i64().is_some()=>(result["message_id"].as_i64(),None),
        Ok(_)=>(None,Some("первый комментарий: Telegram вернул неполный ответ. Он мог уйти, проверьте обсуждение".to_owned())),
        Err(TelegramError::NoAnswer(error))=>(None,Some(format!("первый комментарий: {error}. Он мог уйти, проверьте обсуждение"))),
        Err(error)=>(None,Some(format!("первый комментарий: {error}")))
    };
    let id = post["id"].as_str().unwrap_or("").to_owned();
    let error_copy = error.clone();
    let saved = blocking::run(context, move |context| {
        db::comment(context, &id, mid, error_copy.as_deref())?;
        Ok(posts::get(context, &id)?)
    })
    .await?;
    if error.is_some()
        && let Some(post) = saved
    {
        let _ =
            bot_notifications::record_event(context, &state.settings, &post, "comment", None).await;
    }
    Ok(Some("comment"))
}
