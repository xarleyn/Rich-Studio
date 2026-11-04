use super::definition;
use crate::{
    AppState,
    services::idempotency::{Idempotency, fingerprint},
};
use serde_json::{Value, json};
pub fn definitions() -> Vec<Value> {
    vec![
        definition(
            "list_invite_links",
            "Пригласительные ссылки со счётчиками вступлений и заявок.",
            true,
            false,
            false,
            json!({"chat_id":{"type":["string","integer","null"]}}),
        ),
        definition(
            "create_invite_link",
            "Создать именную пригласительную ссылку или платную подписку.",
            false,
            true,
            true,
            json!({"chat_id":{},"name":{},"expire_date":{},"member_limit":{},"creates_join_request":{},"subscription_price":{}}),
        ),
        definition(
            "edit_invite_link",
            "Изменить ссылку, сохраняя непереданные поля.",
            false,
            true,
            true,
            json!({"invite_link":{},"name":{},"expire_date":{},"member_limit":{},"creates_join_request":{}}),
        ),
        definition(
            "revoke_invite_link",
            "Отозвать ссылку, сохранив счётчики.",
            false,
            true,
            true,
            json!({"invite_link":{"type":"string"}}),
        ),
        definition(
            "list_suggested_posts",
            "Предложения из личных сообщений канала.",
            true,
            false,
            false,
            json!({"state":{"type":["string","null"],"default":"open"}}),
        ),
        definition(
            "approve_suggested_post",
            "Принять предложение после явного подтверждения.",
            false,
            true,
            true,
            json!({"suggestion_id":{"type":"string"},"send_date":{},"idempotency_key":{}}),
        ),
        definition(
            "decline_suggested_post",
            "Отклонить предложение после подтверждения.",
            false,
            true,
            true,
            json!({"suggestion_id":{"type":"string"},"comment":{}}),
        ),
    ]
}
pub async fn call(state: AppState, name: &str, args: Value) -> Result<Value, String> {
    let actor = state.principal.actor("mcp");
    let result = match name {
        "list_invite_links" => {
            crate::services::community::links(&state)
                .await
                .listing(&args["chat_id"])
                .await
        }
        "create_invite_link" => {
            crate::services::community::links(&state)
                .await
                .create(&args, &actor)
                .await
        }
        "edit_invite_link" => {
            crate::services::community::links(&state)
                .await
                .edit(&args)
                .await
        }
        "revoke_invite_link" => {
            crate::services::community::links(&state)
                .await
                .revoke(args["invite_link"].as_str().unwrap_or(""))
                .await
        }
        "list_suggested_posts" => {
            crate::services::community::suggestions(&state)
                .listing(
                    args.get("state")
                        .map_or(Some("open".to_owned()), |v| v.as_str().map(str::to_owned)),
                )
                .await
        }
        "approve_suggested_post" => {
            let id = args["suggestion_id"].as_str().unwrap_or("");
            let body = json!({"sid":id,"send_date":args["send_date"]});
            let key = args["idempotency_key"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("auto:{}", fingerprint("suggested-approve", &body)));
            Idempotency::new(state.account.clone())
                .run(Some(&key), "suggested-approve", &body, &actor, || async {
                    crate::services::community::suggestions(&state)
                        .approve(id, &args["send_date"], &actor)
                        .await
                })
                .await
        }
        "decline_suggested_post" => {
            crate::services::community::suggestions(&state)
                .decline(
                    args["suggestion_id"].as_str().unwrap_or(""),
                    &args["comment"],
                    &actor,
                )
                .await
        }
        _ => return Err(format!("unknown tool {name}")),
    };
    result.map_err(|e| e.to_string())
}
