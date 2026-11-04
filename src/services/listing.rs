use std::collections::VecDeque;

use regex::Regex;
use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::errors::ServiceError;
use crate::services::kinds;
use crate::store::{channels, editing};

fn pattern(source: &str) -> Result<Regex, ServiceError> {
    Regex::new(source).map_err(|error| ServiceError::Internal(error.to_string()))
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(plain).collect(),
        Value::Object(object) => object.get("text").map(plain).unwrap_or_default(),
        _ => String::new(),
    }
}

fn first_block_text(blocks: &Value) -> String {
    for block in blocks.as_array().into_iter().flatten() {
        for key in ["text", "summary", "caption", "expression"] {
            let text = plain(&block[key]);
            if !text.trim().is_empty() {
                return text.trim().to_owned();
            }
        }
        let nested = first_block_text(&block["blocks"]);
        if !nested.is_empty() {
            return nested;
        }
        for item in block["items"].as_array().into_iter().flatten() {
            let nested = first_block_text(&item["blocks"]);
            if !nested.is_empty() {
                return nested;
            }
        }
        for row in block["cells"].as_array().into_iter().flatten() {
            for cell in row.as_array().into_iter().flatten() {
                let text = plain(&cell["text"]);
                if !text.trim().is_empty() {
                    return text.trim().to_owned();
                }
            }
        }
    }
    String::new()
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

pub fn title(payload: &Value, kind: Option<&str>) -> Result<String, ServiceError> {
    let kind = kinds::get(kind)?;
    if let Some(key) = kind.key {
        let text = payload[key][if kind.stoppable {
            "question"
        } else {
            "caption"
        }]
        .as_str()
        .unwrap_or_default();
        if kind.stoppable {
            return Ok(text.to_owned());
        }
        let text = first_line(text);
        return Ok(if text.is_empty() {
            if kind.name == kinds::PAID_MEDIA {
                "Платный контент".to_owned()
            } else {
                kind.label.to_owned()
            }
        } else {
            text
        });
    }
    let rich = &payload["rich_message"];
    let text = if rich["blocks"]
        .as_array()
        .is_some_and(|blocks| !blocks.is_empty())
    {
        first_line(&first_block_text(&rich["blocks"]))
    } else if let Some(html) = rich["html"].as_str() {
        let lines = pattern(r"(?i)<(?:br|hr|/p|/h[1-6]|/li|/blockquote|/figcaption|/summary|/caption|/aside|/pre|/footer)\b[^>]*>")?.replace_all(html,"\n");
        first_line(&html_escape::decode_html_entities(
            &pattern(r"<[^>]+>")?.replace_all(&lines, ""),
        ))
    } else if let Some(markdown) = rich["markdown"].as_str() {
        let prefix = pattern(r"^\s*(?:#{1,6}\s+|>\s*|[-*+]\s+(?:\[[ xX]\]\s+)?|\d+[.)]\s+|\|)")?;
        let marks =
            pattern(r"\*\*|__|~~|\|\||==|`|\$\$?|!\[[^\]]*\]\([^)]*\)|\[([^\]]*)\]\([^)]*\)")?;
        let divider = pattern(r"^[-=*_:| ]+$")?;
        markdown
            .lines()
            .map(|line| {
                marks
                    .replace_all(&prefix.replace(line, ""), "$1")
                    .trim_matches([' ', '|'])
                    .to_owned()
            })
            .find(|line| !line.is_empty() && !divider.is_match(line))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let text = pattern(r"\s+")?.replace_all(&text, " ").into_owned();
    Ok(if text.chars().count() <= 120 {
        text
    } else {
        format!("{}…", text.chars().take(119).collect::<String>().trim_end())
    })
}

pub fn thumb(payload: &Value) -> Option<String> {
    let mut queue: VecDeque<&Value> = std::iter::once(&payload["rich_message"])
        .chain(
            kinds::KINDS
                .iter()
                .filter_map(|kind| kind.key)
                .map(|key| &payload[key]),
        )
        .collect();
    while let Some(value) = queue.pop_front() {
        match value {
            Value::Object(object) => {
                if value["type"] == "photo"
                    && let Some(name) = value["media"]
                        .as_str()
                        .and_then(|media| media.strip_prefix("upload:"))
                {
                    return Some(format!("/media/{name}"));
                }
                if let Some(name) = value["photo"]
                    .as_str()
                    .and_then(|media| media.strip_prefix("upload:"))
                {
                    return Some(format!("/media/{name}"));
                }
                queue.extend(object.values());
            }
            Value::Array(items) => queue.extend(items),
            _ => {}
        }
    }
    None
}

pub fn button_texts(payload: &Value, kind: Option<&str>) -> Result<Value, ServiceError> {
    let kind = kinds::get(kind)?;
    let holder = kind.key.map_or(payload, |key| &payload[key]);
    let mut result = serde_json::Map::new();
    for row in holder["reply_markup"]["inline_keyboard"]
        .as_array()
        .into_iter()
        .flatten()
    {
        for button in row.as_array().into_iter().flatten() {
            if let (Some(data), Some(text)) =
                (button["callback_data"].as_str(), button["text"].as_str())
            {
                result.entry(data.to_owned()).or_insert_with(|| json!(text));
            }
        }
    }
    Ok(Value::Object(result))
}

pub fn link(chat: &Value, message: &Value) -> Option<String> {
    if message.is_null() || message.as_i64() == Some(0) {
        return None;
    }
    let chat = chat
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| chat.to_string());
    if let Some(name) = chat.strip_prefix('@') {
        return Some(format!("https://t.me/{name}/{message}"));
    }
    let id = chat.strip_prefix("-100")?;
    (!id.is_empty() && id.chars().all(|character| character.is_ascii_digit()))
        .then(|| format!("https://t.me/c/{id}/{message}"))
}

pub fn channel_title(context: &AccountContext, chat: &Value) -> Result<Value, ServiceError> {
    let channel = channels::get(context, chat)?;
    Ok(channel
        .as_ref()
        .and_then(|channel| channel["title"].as_str())
        .map_or(Value::Null, |title| json!(title)))
}

pub fn list_posts(
    context: &AccountContext,
    limit: i64,
    status: Option<&str>,
    chat: Option<&str>,
) -> Result<Vec<Value>, ServiceError> {
    let mut records = editing::list(context, limit, status, chat)?;
    for record in &mut records {
        record["chat_title"] = channel_title(context, &record["chat_id"])?;
        let payload = record
            .as_object_mut()
            .and_then(|record| record.remove("payload"))
            .unwrap_or(Value::Null);
        record["title"] = json!(title(&payload, record["kind"].as_str())?);
        record["thumb"] = json!(thumb(&payload));
        record["buttons"] = button_texts(&payload, record["kind"].as_str())?;
        record["link"] = json!(link(&record["chat_id"], &record["message_id"]));
        record["pinned"] = json!(record["pinned"].as_i64().unwrap_or(0) != 0);
    }
    Ok(records)
}
