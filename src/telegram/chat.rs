use serde_json::{Map, Value, json};

use crate::account_context::AccountContext;
use crate::telegram::TelegramError;
use crate::telegram::client::TelegramTransport;
use crate::telegram::uploads::Uploads;

pub async fn check_chat(
    transport: &TelegramTransport,
    context: &AccountContext,
    chat_id: Value,
) -> Result<Value, TelegramError> {
    let me = transport
        .call(context, "getMe", json!({}), Uploads::default())
        .await?;
    let chat = transport
        .call(
            context,
            "getChat",
            json!({"chat_id":chat_id}),
            Uploads::default(),
        )
        .await?;
    let member = transport
        .call(
            context,
            "getChatMember",
            json!({"chat_id":chat_id,"user_id":me["id"]}),
            Uploads::default(),
        )
        .await?;
    let rights = member
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|(key, _)| key.starts_with("can_"))
        .collect::<Map<_, _>>();
    let chat_type = chat.get("type").and_then(Value::as_str).unwrap_or_default();
    let can_post = member.get("status").and_then(Value::as_str) == Some("creator")
        || member.get("can_post_messages").and_then(Value::as_bool) == Some(true)
        || ["group", "supergroup", "private"].contains(&chat_type);
    Ok(json!({
        "bot":{"id":me["id"],"username":me["username"]},
        "chat":{"id":chat["id"],"title":chat["title"],"type":chat["type"],"username":chat["username"],"is_forum":chat["is_forum"].as_bool().unwrap_or(false)},
        "status":member["status"],
        "rights":rights,
        "can_post":can_post,
    }))
}
