use serde_json::{Value, json};

use crate::AppState;
use crate::errors::ServiceError;
use crate::services::idempotency::{Idempotency, fingerprint};
use crate::services::kinds;

use super::definition;

pub fn definitions() -> Vec<Value> {
    vec![definition(
        "send_media_post",
        "Отправить Live Photo или платное медиа после явного подтверждения пользователя.",
        false,
        true,
        true,
        json!({
            "kind":{"type":"string","enum":[kinds::LIVE_PHOTO,kinds::PAID_MEDIA]},
            "params":{"type":"object"},
            "chat_id":{"type":["string","integer"]},
            "chat_ids":{"type":"array"},
            "at":{"type":["string","integer"]},
            "pin":{"type":"boolean"},
            "idempotency_key":{"type":"string"}
        }),
    )]
}

pub async fn call(state: AppState, arguments: Value) -> Result<Value, String> {
    send(state, arguments)
        .await
        .map_err(|error| error.to_string())
}

async fn send(state: AppState, arguments: Value) -> Result<Value, ServiceError> {
    let kind_name = arguments["kind"].as_str().unwrap_or_default();
    if ![kinds::LIVE_PHOTO, kinds::PAID_MEDIA].contains(&kind_name) {
        return Err(ServiceError::BadRequest(
            "kind: live_photo или paid_media".to_owned(),
        ));
    }
    let kind = kinds::get(Some(kind_name))?;
    let body = json!({
        "kind":kind_name,
        "params":arguments["params"],
        "chat_id":arguments["chat_id"],
        "chat_ids":arguments["chat_ids"],
        "at":arguments["at"],
        "pin":arguments["pin"].as_bool().unwrap_or(false),
    });
    let key = arguments["idempotency_key"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("auto:{}", fingerprint("media-post", &body)));
    let action_body = body.clone();
    let service = crate::services::posts::from_state(&state).await;
    let actor = state.principal.actor("mcp");
    let action_actor = actor.clone();
    Idempotency::new(state.account.clone())
        .run(Some(&key), "media-post", &body, &actor, || async move {
            service
                .send_kind(
                    kind,
                    &action_body["params"],
                    &action_body["chat_id"],
                    &action_body["chat_ids"],
                    &action_body["at"],
                    action_body["pin"].as_bool().unwrap_or(false),
                    &action_actor,
                )
                .await
        })
        .await
}
