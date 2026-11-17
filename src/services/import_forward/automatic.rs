//! Import an owner's/account's private forward without first requesting a link.
//! File IDs stay in Telegram; source notes and album state stay in this account.

use super::sender_context;
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ImportError,
    services::{bot_dialog::private_sender, listing},
    spec::{self, import_content::entities_text, source::Spec},
    store::{db, drafts, meta},
};
use serde_json::{Map, Value, json};

const ALBUMS_KEY: &str = "automatic_forward_albums";
const RECENT_KEY: &str = "automatic_forward_recent";
const BOT_ACTIONS: &[&str] = &[
    "callback_data",
    "callback_game",
    "pay",
    "web_app",
    "login_url",
    "switch_inline_query",
    "switch_inline_query_current_chat",
    "switch_inline_query_chosen_chat",
];

struct Content {
    media: Vec<Value>,
    text: Vec<Value>,
    rich: Map<String, Value>,
    markup: Option<Value>,
    warnings: Vec<String>,
}

fn bot_only(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| BOT_ACTIONS.iter().any(|key| object.contains_key(*key)))
}

fn strip_buttons(value: Value, dropped: &mut usize) -> Option<Value> {
    match value {
        Value::Array(items) => {
            let mut kept = Vec::new();
            for item in items {
                if bot_only(&item) {
                    *dropped += 1;
                    continue;
                }
                if let Some(item) = strip_buttons(item, dropped) {
                    kept.push(item);
                }
            }
            Some(Value::Array(kept))
        }
        Value::Object(mut object) => {
            if object.get("type") == Some(&json!("button"))
                && object.get("button").is_some_and(bot_only)
            {
                *dropped += 1;
                return Some(
                    object["button"]
                        .get("text")
                        .cloned()
                        .unwrap_or_else(|| json!("")),
                );
            }
            for value in object.values_mut() {
                *value = strip_buttons(std::mem::take(value), dropped).unwrap_or(Value::Null);
            }
            if object.get("type") == Some(&json!("buttons"))
                && object
                    .get("buttons")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty)
            {
                return None;
            }
            Some(Value::Object(object))
        }
        other => Some(other),
    }
}

fn media_fields(spec: &Spec) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for name in spec.types()["InputRichBlock"]["subtypes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let Some((_, kind)) = spec::type_const(spec.types(), name) else {
            continue;
        };
        for field in spec.types()[name]["fields"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if field["types"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|name| name.starts_with("InputMedia"))
                && let Some(key) = field["name"].as_str()
            {
                found.push((kind.clone(), key.to_owned()));
            }
        }
    }
    found
}

fn content(spec: &Spec, message: &Value) -> Result<Content, ImportError> {
    if message["has_protected_content"] == true {
        return Err(ImportError::new(
            422,
            "у поста защищённое содержимое, его нельзя копировать",
        ));
    }
    let mut result = Content {
        media: Vec::new(),
        text: Vec::new(),
        rich: Map::new(),
        markup: None,
        warnings: Vec::new(),
    };
    if let Some(rich) = message["rich_message"].as_object() {
        result.text = spec::convert::to_input_blocks(
            spec,
            rich.get("blocks")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        for field in spec.types()["InputRichMessage"]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|field| field["name"].as_str())
        {
            if field != "blocks"
                && let Some(value) = rich.get(field)
            {
                result.rich.insert(field.to_owned(), value.clone());
            }
        }
    } else {
        let text = message["text"]
            .as_str()
            .or_else(|| message["caption"].as_str())
            .unwrap_or("");
        if !text.is_empty() {
            let entities = message["entities"]
                .as_array()
                .or_else(|| message["caption_entities"].as_array())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            result.text.push(json!({"type":"paragraph","text":entities_text(text,entities,&mut result.warnings)?}));
        }
        let media_fields = media_fields(spec);
        for (kind, key) in &media_fields {
            let message_key = if key == "voice_note" { "voice" } else { key };
            if message
                .get(message_key)
                .is_some_and(|value| !value.is_null())
            {
                let mut block =
                    json!({"type":kind,"has_spoiler":message["has_media_spoiler"] == true});
                block[key] = message[message_key].clone();
                result.media = spec::convert::to_input_blocks(spec, &[block]);
                break;
            }
        }
        for field in spec.types()["Message"]["fields"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(key) = field["name"].as_str() else {
                continue;
            };
            let taken = key == "rich_message"
                || media_fields.iter().any(|(_, media)| {
                    if media == "voice_note" {
                        key == "voice"
                    } else {
                        key == media
                    }
                });
            let description = field["description"].as_str().unwrap_or("");
            let content = [
                "Optional. Message is ",
                "Optional. The message is ",
                "Optional. Message contains ",
                "Optional. The message contains ",
            ]
            .iter()
            .any(|prefix| description.starts_with(prefix));
            if !taken
                && content
                && message
                    .get(key)
                    .is_some_and(|value| !value.is_null() && value != &json!(false))
            {
                result.warnings.push(format!(
                    "не перенесено, в rich-посте для этого нет блока: {key}"
                ));
            }
        }
    }
    let mut dropped = 0;
    result.media = strip_buttons(json!(result.media), &mut dropped)
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    result.text = strip_buttons(json!(result.text), &mut dropped)
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    if let Some(rows) = message["reply_markup"]["inline_keyboard"].as_array() {
        let rows: Vec<_> = rows
            .iter()
            .filter_map(|row| strip_buttons(row.clone(), &mut dropped))
            .filter(|row| row.as_array().is_some_and(|row| !row.is_empty()))
            .collect();
        if !rows.is_empty() {
            result.markup = Some(json!({"inline_keyboard":rows}));
        }
    }
    if dropped > 0 {
        result.warnings.push(format!("убраны кнопки ({dropped}), которые работают только у бота исходного поста; ссылки и копирование текста сохранены"));
    }
    if result.media.is_empty() && result.text.is_empty() {
        return Err(ImportError::new(
            422,
            "нечего перенести: нет текста, фото, видео, файла или rich-блоков после удаления кнопок исходного бота",
        ));
    }
    Ok(result)
}

fn blocks(media: &[Value], text: &[Value], above: bool) -> Vec<Value> {
    let media = if media.len() > 1
        && media
            .iter()
            .all(|block| matches!(block["type"].as_str(), Some("photo" | "video")))
    {
        vec![json!({"type":"collage","blocks":media})]
    } else {
        media.to_vec()
    };
    if above {
        text.iter().cloned().chain(media).collect()
    } else {
        media.into_iter().chain(text.iter().cloned()).collect()
    }
}

fn source(message: &Value) -> Value {
    let origin = &message["forward_origin"];
    let mut source =
        json!({"message_id":message["message_id"],"date":message["date"],"provider":"forward"});
    if !origin.is_null() {
        source["forward_origin"] = origin.clone();
    }
    if origin["type"] == "channel"
        && let Some(mid) = origin["message_id"].as_i64()
    {
        if let Some(username) = origin["chat"]["username"].as_str() {
            source["link"] = json!(format!("https://t.me/{username}/{mid}"));
        } else if let Some(chat) = origin["chat"]["id"]
            .as_i64()
            .map(|id| id.to_string())
            .and_then(|id| id.strip_prefix("-100").map(str::to_owned))
        {
            source["link"] = json!(format!("https://t.me/c/{chat}/{mid}"));
        }
    }
    source
}

fn load_list(context: &AccountContext, key: &str) -> Result<Vec<Value>, ImportError> {
    match meta::get(context, key)? {
        Some(raw) => serde_json::from_str::<Vec<Value>>(&raw)
            .map_err(|_| ImportError::new(500, "Состояние пересылок повреждено.")),
        None => Ok(Vec::new()),
    }
}

fn save(context: &AccountContext, spec: &Spec, message: &Value) -> Result<Value, ImportError> {
    db::migrate(&mut db::Store::connect(context)?)?;
    let identity = format!("{}:{}", message["from"]["id"], message["message_id"]);
    let mut recent = load_list(context, RECENT_KEY)?;
    if let Some(previous) = recent.iter().find(|entry| entry["message"] == identity) {
        return Ok(previous["draft"].clone());
    }
    let content = content(spec, message)?;
    let above = message["show_caption_above_media"] == true;
    let group = message["media_group_id"].as_str();
    let mut albums = load_list(context, ALBUMS_KEY)?;
    let mut saved = None;
    if let Some(group) = group
        && let Some(album) = albums.iter_mut().find(|album| album["group"] == group)
        && let Some(id) = album["draft"].as_str()
        && let Some(mut draft) = drafts::get(context, id)?
        && draft["version"] == album["version"]
    {
        let mut media = album["media"].as_array().cloned().unwrap_or_default();
        media.extend(content.media.clone());
        let mut text = album["text"].as_array().cloned().unwrap_or_default();
        text.extend(content.text.clone());
        draft["payload"]["rich_message"]["blocks"] =
            json!(blocks(&media, &text, album["above"] == true));
        let mut warnings = draft["payload"]["_studio"]["warnings"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for warning in &content.warnings {
            let warning = json!(warning);
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
        draft["payload"]["_studio"]["warnings"] = json!(warnings);
        let chats = draft["chat_ids"].as_array().map(|chats| {
            chats
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        saved = drafts::save(
            context,
            Some(id),
            draft["title"].as_str().unwrap_or("Пересылка"),
            &draft["payload"],
            draft["version"].as_i64(),
            draft["post_id"].as_str(),
            chats.as_deref(),
        )?;
        if let Some(draft) = &saved {
            album["version"] = draft["version"].clone();
            album["media"] = json!(media);
            album["text"] = json!(text);
        }
    }
    if saved.is_none() {
        let mut rich = content.rich;
        rich.insert(
            "blocks".to_owned(),
            json!(blocks(&content.media, &content.text, above)),
        );
        let mut payload = json!({"rich_message":rich,"options":{},"_studio":{"source":source(message),"warnings":content.warnings,"import_source":source(message),"import_warnings":content.warnings,"format":"blocks"}});
        if let Some(markup) = content.markup {
            payload["reply_markup"] = markup;
        }
        let title = listing::title(&payload, Some("rich"))
            .map_err(|error| ImportError::new(500, error.to_string()))?;
        saved = drafts::save(
            context,
            None,
            if title.is_empty() {
                "Пересылка"
            } else {
                &title
            },
            &payload,
            None,
            None,
            None,
        )?;
        if let (Some(group), Some(draft)) = (group, &saved) {
            albums.retain(|album| album["group"] != group);
            albums.push(json!({"group":group,"draft":draft["id"],"version":draft["version"],"media":content.media,"text":content.text,"above":above}));
        }
    }
    let saved = saved.ok_or_else(|| ImportError::new(500, "Не удалось сохранить пересылку."))?;
    if albums.len() > 5 {
        albums.drain(..albums.len() - 5);
    }
    if let Err(error) = meta::set(context, ALBUMS_KEY, &json!(albums).to_string()) {
        tracing::warn!(error=%error,"forward draft saved but album bookkeeping could not be saved");
    }
    recent.push(json!({"message":identity,"draft":saved["id"]}));
    if recent.len() > 128 {
        recent.drain(..recent.len() - 128);
    }
    if let Err(error) = meta::set(context, RECENT_KEY, &json!(recent).to_string()) {
        tracing::warn!(error=%error,"forward draft saved but recent import bookkeeping could not be saved");
    }
    Ok(saved["id"].clone())
}

pub async fn receive_automatic(
    state: &AppState,
    update: &Value,
    spec: &Spec,
) -> Result<bool, ImportError> {
    let Some(sender) = private_sender(update) else {
        return Ok(false);
    };
    let message = &update["message"];
    if (message["forward_origin"].is_null() && message["rich_message"].is_null())
        || message["message_id"].as_i64().is_none_or(|id| id <= 0)
    {
        return Ok(false);
    }
    let settings = state.settings.clone();
    let message_copy = message.clone();
    let spec = spec.clone();
    let result = tokio::task::spawn_blocking(move || {
        let Some(context) = sender_context(&settings, sender)? else {
            return Ok(None);
        };
        let result = save(&context, &spec, &message_copy);
        Ok::<_, ImportError>(Some((context, result)))
    })
    .await
    .map_err(|_| ImportError::new(500, "Обработка пересылки прервана."))??;
    let Some((context, result)) = result else {
        return Ok(false);
    };
    let reason = match &result {
        Ok(id) => {
            state.logs.event(
                "INFO",
                "rich-studio.forward_import",
                "forward saved as draft",
                json!({"event":"forward.import","draft_id":id}),
            );
            None
        }
        Err(error) => {
            state.logs.event(
                "WARNING",
                "rich-studio.forward_import",
                &error.to_string(),
                json!({"event":"forward.refused"}),
            );
            Some(if error.status < 500 {
                error.to_string()
            } else {
                "ошибка студии, подробности в журнале".to_owned()
            })
        }
    };
    if let Err(error) = state
        .transport()
        .answer_forward_import(
            &context,
            sender,
            message["message_id"].as_i64().unwrap_or(0),
            reason.as_deref(),
        )
        .await
    {
        state.logs.event(
            "WARNING",
            "rich-studio.forward_import",
            &error.to_string(),
            json!({"event":"forward.answer_failed"}),
        );
    }
    Ok(true)
}
