use serde_json::{Map, Value, json};

use crate::AppState;
use crate::services::drafts;
use crate::services::drafts::{DraftService, SaveDraft, draft_links};

use super::definition;

pub fn definitions() -> Vec<Value> {
    vec![
        definition(
            "validate_post",
            "Проверить пост по спецификации: ошибки, предупреждения, длина.",
            true,
            false,
            false,
            json!({"post": {"type": "object"}}),
        ),
        definition(
            "save_draft",
            "Сохранить черновик и сразу проверить его; вернуть ссылки для редактора и предпросмотра.",
            false,
            false,
            false,
            json!({"post": {"type": "object"}, "title": {"type": "string"}, "draft_id": {"type": "string"}, "version": {"type": "integer"}, "chat_ids": {"type": "array", "items": {"type": "string"}}}),
        ),
        definition(
            "list_drafts",
            "Черновики студии, последние сверху.",
            true,
            false,
            false,
            json!({}),
        ),
        definition(
            "get_draft",
            "Черновик целиком: пост, название и version для save_draft.",
            true,
            false,
            false,
            json!({"draft_id": {"type": "string"}}),
        ),
    ]
}

fn with_links(state: &AppState, mut draft: Value) -> Result<Value, String> {
    let id = draft
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut object = draft.as_object().cloned().unwrap_or_default();
    object.insert(
        "goes_to".to_owned(),
        crate::services::channels::goes_to(
            &state.account,
            &state.settings.default_chat,
            &draft["chat_ids"],
        )
        .map_err(|error| error.to_string())?,
    );
    if let Some(links) = draft_links(&state.settings.public_url, &id).as_object() {
        object.extend(links.clone());
    }
    draft = Value::Object(object);
    Ok(draft)
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    match name {
        "save_draft" => {
            let id = arguments
                .get("draft_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let old = match drafts::run(state.account.clone(), {
                let id = id.clone();
                move |service| match id {
                    None => Ok(None),
                    Some(id) => match service.get(&id) {
                        Ok(draft) => Ok(Some(draft)),
                        Err(drafts::DraftError::NotFound(_)) => Ok(None),
                        Err(error) => Err(error),
                    },
                }
            })
            .await
            {
                Ok(old) => old,
                Err(error) => return Err(error.to_string()),
            };
            if id.is_some() && old.is_none() {
                return Err(format!("черновика {} нет", id.unwrap_or_default()));
            }
            let input = SaveDraft {
                id,
                title: arguments
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        old.as_ref()
                            .and_then(|draft| draft.get("title"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default(),
                post: crate::services::posts::normalize_post(&arguments["post"]),
                version: arguments.get("version").and_then(Value::as_i64),
                post_id: old
                    .as_ref()
                    .and_then(|draft| draft.get("post_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                chat_ids: arguments
                    .get("chat_ids")
                    .and_then(Value::as_array)
                    .map(|chats| {
                        chats
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    }),
            };
            let draft = drafts::run(state.account.clone(), move |service| service.save(input))
                .await
                .map_err(|error| error.to_string())?;
            let spec = state.spec.read().await.clone();
            let validation = super::bot_api::validate(&spec, &draft["payload"]);
            let mut output = Map::new();
            for field in ["id", "title", "version", "chat_ids"] {
                output.insert(
                    field.to_owned(),
                    draft.get(field).cloned().unwrap_or(Value::Null),
                );
            }
            output.insert(
                "preview_path".to_owned(),
                json!(format!(
                    "/render/draft/{}",
                    draft["id"].as_str().unwrap_or_default()
                )),
            );
            output.insert("validation".to_owned(), validation);
            tokio::task::spawn_blocking(move || with_links(&state, Value::Object(output)))
                .await
                .map_err(|_| "операция черновика прервана".to_owned())?
        }
        "list_drafts" => {
            let drafts = drafts::run(state.account.clone(), DraftService::list)
                .await
                .map_err(|error| error.to_string())?;
            tokio::task::spawn_blocking(move || {
                drafts
                    .iter()
                    .map(|draft| with_links(&state, draft.clone()))
                    .collect::<Result<Vec<_>, String>>()
                    .map(Value::Array)
            })
            .await
            .map_err(|_| "операция черновиков прервана".to_owned())?
        }
        "get_draft" => {
            let id = arguments
                .get("draft_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let draft = drafts::run(state.account.clone(), move |service| service.get(&id))
                .await
                .map_err(|error| error.to_string())?;
            tokio::task::spawn_blocking(move || with_links(&state, draft))
                .await
                .map_err(|_| "операция черновика прервана".to_owned())?
        }
        "send_preview_to_me" => {
            let post = crate::services::posts::normalize_post(&arguments["post"]);
            let mut result = crate::services::posts::from_state(&state)
                .await
                .preview(&post, &Value::Null)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(parsed) = result["parsed"].as_object_mut() {
                parsed.remove("blocks");
            }
            Ok(result)
        }
        _ => Err(format!("unknown draft tool {name}")),
    }
}
