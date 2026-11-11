use crate::errors::ImportError;
use grammers_tl_types as tl;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Normalize serde's externally tagged TL enums to Telethon's constructor-tagged data.
/// Prefixes are selected by schema position; text constructors retain their exact TL names.
pub fn normalize_tl(value: Value, prefix: &str) -> Value {
    match value {
        Value::String(name) if !prefix.is_empty() => {
            json!({"_":if name.starts_with("Text")||name.starts_with(prefix){name}else{format!("{prefix}{name}")}})
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| normalize_tl(item, prefix))
                .collect(),
        ),
        Value::Object(mut object) => {
            if !prefix.is_empty() && object.len() == 1 {
                let key = object.keys().next().cloned().unwrap_or_default();
                if key.chars().next().is_some_and(char::is_uppercase) {
                    let child = object.remove(&key).unwrap_or(Value::Null);
                    let mut output = match child {
                        Value::Object(child) => child,
                        _ => serde_json::Map::new(),
                    };
                    output.insert(
                        "_".into(),
                        json!(if key.starts_with("Text") || key.starts_with(prefix) {
                            key
                        } else {
                            format!("{prefix}{key}")
                        }),
                    );
                    object = output;
                }
            }
            let constructor = object
                .get("_")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            for (key, child) in object.iter_mut() {
                if key == "_" {
                    continue;
                }
                let nested = match key.as_str() {
                    "blocks" | "cover" => "PageBlock",
                    "text" if constructor == "TextPlain" => "",
                    "text" | "texts" | "title" | "credit" => "Text",
                    "caption"
                        if [
                            "PageBlockBlockquote",
                            "PageBlockBlockquoteBlocks",
                            "PageBlockPullquote",
                        ]
                        .contains(&constructor.as_str()) =>
                    {
                        "Text"
                    }
                    "caption" => "PageCaption",
                    "rows" => "PageTableRow",
                    "cells" => "PageTableCell",
                    "items" if constructor == "PageBlockList" => "PageListItem",
                    "items" if constructor == "PageBlockOrderedList" => "PageListOrderedItem",
                    "items"
                        if ["PageBlockCollage", "PageBlockSlideshow"]
                            .contains(&constructor.as_str()) =>
                    {
                        "PageBlock"
                    }
                    "geo" => "GeoPoint",
                    _ => "",
                };
                *child = normalize_tl(std::mem::take(child), nested);
            }
            Value::Object(object)
        }
        other => other,
    }
}
pub(super) fn tl_value<T: Serialize>(value: &T, prefix: &str) -> Result<Value, ImportError> {
    serde_json::to_value(value)
        .map(|value| normalize_tl(value, prefix))
        .map_err(|_| ImportError::new(422, "Неверное содержимое Telegram."))
}

pub fn mt_entities(values: &[tl::enums::MessageEntity]) -> Result<Vec<Value>, ImportError> {
    values
        .iter()
        .map(|entity| {
            let mut value = tl_value(entity, "MessageEntity")?;
            let name = value["_"]
                .as_str()
                .unwrap_or("")
                .trim_start_matches("MessageEntity");
            let kind = match name {
                "Strike" => "strikethrough".to_owned(),
                "TextUrl" => "text_link".to_owned(),
                "MentionName" => "text_mention".to_owned(),
                _ => name
                    .chars()
                    .enumerate()
                    .flat_map(|(index, ch)| {
                        if ch.is_uppercase() && index > 0 {
                            vec!['_', ch.to_ascii_lowercase()]
                        } else {
                            vec![ch.to_ascii_lowercase()]
                        }
                    })
                    .collect(),
            };
            if let Some(object) = value.as_object_mut() {
                object.remove("_");
                object.insert("type".into(), json!(kind));
                if let Some(id) = object.remove("document_id") {
                    object.insert("custom_emoji_id".into(), json!(id.to_string()));
                }
            }
            Ok(value)
        })
        .collect()
}

pub(super) fn referenced_media(value: &Value, needed: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                referenced_media(item, needed)
            }
        }
        Value::Object(object) => {
            if [
                "PageBlockPhoto",
                "PageBlockVideo",
                "PageBlockAudio",
                "PageBlockDocument",
            ]
            .contains(&value["_"].as_str().unwrap_or(""))
            {
                for field in ["photo_id", "video_id", "audio_id", "document_id"] {
                    if let Some(id) = object.get(field) {
                        needed.insert(id.to_string());
                    }
                }
            }
            for child in object.values() {
                referenced_media(child, needed)
            }
        }
        _ => {}
    }
}
