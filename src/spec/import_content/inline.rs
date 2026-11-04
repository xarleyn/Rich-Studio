use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::errors::ImportError;

use super::{invalid, string, text};

pub fn safe_link(value: &str) -> String {
    if value
        .chars()
        .any(|ch| ch <= '\u{20}' || ch == '\u{7f}' || ch == '\\')
    {
        return String::new();
    }
    if value.starts_with('#') {
        return value.to_owned();
    }
    if let Ok(url) = url::Url::parse(value) {
        if ["http", "https"].contains(&url.scheme())
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && value.contains("://")
        {
            return value.to_owned();
        }
        if ["mailto", "tel"].contains(&url.scheme()) && !url.path().is_empty() {
            return value.to_owned();
        }
    }
    String::new()
}

pub fn plain(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(items) => items.iter().map(plain).collect(),
        Value::Object(object) => object
            .get("text")
            .or_else(|| object.get("alternative_text"))
            .or_else(|| object.get("expression"))
            .map(plain)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub(super) fn inline_kind(name: &str) -> Option<&'static str> {
    match name {
        "Bold" => Some("bold"),
        "Italic" => Some("italic"),
        "Underline" => Some("underline"),
        "Strike" => Some("strikethrough"),
        "Spoiler" => Some("spoiler"),
        "Fixed" => Some("code"),
        "Subscript" => Some("subscript"),
        "Superscript" => Some("superscript"),
        "Marked" => Some("marked"),
        _ => None,
    }
}

pub(super) fn tag(kind: &str) -> Option<&'static str> {
    match kind {
        "bold" => Some("b"),
        "italic" => Some("i"),
        "underline" => Some("u"),
        "strikethrough" => Some("s"),
        "spoiler" => Some("tg-spoiler"),
        "code" => Some("code"),
        "subscript" => Some("sub"),
        "superscript" => Some("sup"),
        "marked" => Some("mark"),
        _ => None,
    }
}

pub fn entities_text(
    text: &str,
    entities: &[Value],
    warnings: &mut Vec<String>,
) -> Result<Value, ImportError> {
    if text.chars().count() > 100_000 || entities.len() > 2000 {
        return Err(invalid(
            "Слишком много текста или диапазонов форматирования для импорта.",
        ));
    }
    let raw = text.encode_utf16().collect::<Vec<_>>();
    let mut boundaries = BTreeSet::from([0, raw.len()]);
    let mut valid = Vec::new();
    for e in entities {
        let offset = e["offset"].as_i64().unwrap_or(-1);
        let length = e["length"].as_i64().unwrap_or(0);
        let end = offset.checked_add(length).unwrap_or(-1);
        if offset < 0
            || end <= offset
            || end > raw.len() as i64
            || String::from_utf16(&raw[..offset.max(0).min(raw.len() as i64) as usize]).is_err()
            || String::from_utf16(&raw[..end.max(0).min(raw.len() as i64) as usize]).is_err()
        {
            warnings.push("Некорректный диапазон форматирования пропущен.".to_owned());
            continue;
        }
        let a = offset as usize;
        let b = end as usize;
        boundaries.extend([a, b]);
        valid.push((a, b, e));
    }
    valid.sort_by_key(|(a, b, _)| b - a);
    let points = boundaries.into_iter().collect::<Vec<_>>();
    let mut out = Vec::new();
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let chunk =
            String::from_utf16(&raw[a..b]).map_err(|_| invalid("Некорректный UTF-16 текст."))?;
        let mut node = json!(chunk);
        for (start, end, e) in &valid {
            if *start > a || *end < b {
                continue;
            }
            let kind = super::text(&e["type"]);
            match kind {
                "bold" | "italic" | "underline" | "strikethrough" | "spoiler" | "code" | "pre" => {
                    node = json!({"type":if kind=="pre"{"code"}else{kind},"text":node})
                }
                "text_link" | "url" => {
                    let url = if !super::text(&e["url"]).is_empty() {
                        string(&e["url"])
                    } else {
                        String::from_utf16(&raw[*start..*end])
                            .map_err(|_| invalid("Некорректный UTF-16 текст."))?
                    };
                    if !safe_link(&url).is_empty() {
                        node = json!({"type":"url","url":url,"text":node});
                    }
                }
                "custom_emoji" => {
                    node = json!({"type":"custom_emoji","custom_emoji_id":string(&e["custom_emoji_id"]),"alternative_text":chunk})
                }
                "mention" | "hashtag" | "cashtag" | "bot_command" | "email" | "phone_number" => {}
                _ => warnings.push(format!("Форматирование {kind} перенесено как текст.")),
            }
        }
        out.push(node);
    }
    Ok(if out.is_empty() {
        json!(text)
    } else {
        json!(out)
    })
}

pub fn escape_html(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn text_html(value: &Value) -> String {
    if let Some(value) = value.as_str() {
        return escape_html(value).replace('\n', "<br>");
    }
    if let Some(items) = value.as_array() {
        return items.iter().map(text_html).collect();
    }
    if !value.is_object() {
        return String::new();
    }
    let kind = text(&value["type"]);
    let content = text_html(&value["text"]);
    if let Some(tag) = tag(kind) {
        return format!("<{tag}>{content}</{tag}>");
    }
    match kind {
        "url" if !safe_link(text(&value["url"])).is_empty() => format!(
            "<a href=\"{}\">{content}</a>",
            escape_html(text(&value["url"]))
        ),
        "custom_emoji" => format!(
            "<tg-emoji emoji-id=\"{}\">{}</tg-emoji>",
            escape_html(&string(&value["custom_emoji_id"])),
            escape_html(text(&value["alternative_text"]))
        ),
        "mathematical_expression" => format!(
            "<tg-math>{}</tg-math>",
            escape_html(text(&value["expression"]))
        ),
        "anchor" => format!("<a name=\"{}\"></a>", escape_html(text(&value["name"]))),
        _ => {
            if content.is_empty() {
                escape_html(&plain(value))
            } else {
                content
            }
        }
    }
}
