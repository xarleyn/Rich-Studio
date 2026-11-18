//! Chat restrictions are read from the active Bot API descriptions, including future fields.
use std::collections::{BTreeMap, BTreeSet};

use regex::Regex;
use serde_json::{Value, json};

use super::{source::Spec, type_const};

fn description(value: &Value) -> String {
    value.as_str().map(str::to_owned).unwrap_or_else(|| {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    })
}

fn read(spec: &Spec, text: &str) -> Option<Value> {
    let lower = text.to_lowercase();
    let mut refused = BTreeSet::new();
    for phrase in [
        "only in private chats",
        "in private chats only",
        "for private chats only",
    ] {
        if lower.contains(phrase) {
            refused.extend(["group", "supergroup", "channel"]);
        }
    }
    if lower.contains("not supported in channels") {
        refused.insert("channel");
    }
    if lower.contains("for direct messages chats only") {
        refused.extend(["private", "group", "channel"]);
    }
    if lower.contains("not supported for") && lower.contains("ephemeral messages") {
        refused.insert("ephemeral");
    }
    let only_in = text
        .split("may be used only in ")
        .nth(1)
        .and_then(|tail| {
            tail.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                .next()
        })
        .filter(|method| *method != "sendRichMessage" && spec.methods().get(*method).is_some());
    if refused.is_empty() && only_in.is_none() {
        return None;
    }
    let refused = refused.into_iter().map(|chat| (chat.to_owned(), json!({"why":text,"answer":null,"text":format!("в спецификации ({}): «{}»",spec.version(),text)}))).collect::<BTreeMap<_,_>>();
    Some(json!({"refused":refused,"only_in":only_in,"why":only_in.map(|_|text),"hint":null}))
}

pub fn summary(spec: &Spec) -> Value {
    let mut rules = BTreeMap::new();
    for (owner, definition) in spec.types().as_object().into_iter().flatten().chain(
        spec.methods()
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(name, _)| *name == "sendRichMessage"),
    ) {
        if let Some(rule) = read(spec, &description(&definition["description"])) {
            rules.insert(owner.clone(), rule);
        }
        for field in definition["fields"].as_array().into_iter().flatten() {
            if let Some(rule) = read(spec, &description(&field["description"])) {
                rules.insert(
                    format!("{owner}.{}", field["name"].as_str().unwrap_or("")),
                    rule,
                );
            }
        }
    }
    // Refusals measured against Bot API 10.4 on 2026-11-16 upstream (#166).
    // Retire a measurement when its owning type or field disappears from the active spec.
    for (key, chats, answer) in [
        (
            "sendRichMessage.message_effect_id",
            &["channel", "supergroup"][..],
            "can't use message effects in the chat",
        ),
        (
            "InlineKeyboardButton.web_app",
            &["channel", "supergroup"][..],
            "BUTTON_TYPE_INVALID",
        ),
        (
            "InlineKeyboardButton.callback_game",
            &["channel", "supergroup"][..],
            "BUTTON_TYPE_INVALID",
        ),
        (
            "InlineKeyboardButton.switch_inline_query",
            &["channel"][..],
            "can't use inline button in a channel chat",
        ),
        (
            "InlineKeyboardButton.switch_inline_query_current_chat",
            &["channel"][..],
            "can't use inline button in a channel chat",
        ),
        (
            "InlineKeyboardButton.switch_inline_query_chosen_chat",
            &["channel"][..],
            "can't use switch_inline_query button in a channel chat",
        ),
        (
            "RichMessageButton.web_app",
            &["channel", "supergroup"][..],
            "BUTTON_TYPE_INVALID",
        ),
        (
            "RichMessageButton.switch_inline_query",
            &["channel"][..],
            "RICH_MESSAGE_BLOCK_INVALID",
        ),
        (
            "RichMessageButton.switch_inline_query_current_chat",
            &["channel"][..],
            "RICH_MESSAGE_BLOCK_INVALID",
        ),
        (
            "RichMessageButton.switch_inline_query_chosen_chat",
            &["channel"][..],
            "RICH_MESSAGE_BLOCK_INVALID",
        ),
        (
            "ReplyKeyboardMarkup",
            &["channel"][..],
            "inline keyboard expected",
        ),
        (
            "ReplyKeyboardRemove",
            &["channel"][..],
            "inline keyboard expected",
        ),
        ("ForceReply", &["channel"][..], "inline keyboard expected"),
        (
            "InputRichBlockThinking",
            &["channel", "supergroup"][..],
            "RICH_MESSAGE_BLOCK_UNSUPPORTED",
        ),
    ] {
        let (owner, field) = key
            .split_once('.')
            .map_or((key, None), |(owner, field)| (owner, Some(field)));
        let definition = if owner == "sendRichMessage" {
            &spec.methods()[owner]
        } else {
            &spec.types()[owner]
        };
        if definition.is_null()
            || field.is_some_and(|name| {
                !definition["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|field| field["name"] == name)
            })
        {
            continue;
        }
        let rule = rules
            .entry(key.to_owned())
            .or_insert_with(|| json!({"refused":{},"only_in":null,"why":null,"hint":null}));
        for chat in chats {
            let why = rule["refused"][*chat]["why"].clone();
            let stated = why
                .as_str()
                .map(|why| format!("в спецификации ({}): «{why}»; ", spec.version()))
                .unwrap_or_default();
            rule["refused"][*chat] = json!({"why":why,"answer":answer,"text":format!("{stated}Telegram отклоняет пост: «{answer}» (замер: Bot API 10.4, 2026-11-16)")});
        }
        if key.ends_with(".web_app") {
            rule["hint"] = json!(
                "Используйте кнопку-ссылку url с прямой ссылкой на Mini App https://t.me/<бот>/<приложение>?startapp=…"
            );
        }
    }
    json!({"places":{"channel":"в канале","supergroup":"в группе","forum":"в форуме","group":"в группе","private":"в личке"},"rules":rules})
}

fn types(field: &Value) -> Vec<String> {
    field["types"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn matching_type<'a>(
    spec: &'a Spec,
    value: &Value,
    names: &[String],
    seen: &mut BTreeSet<String>,
) -> Option<&'a str> {
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some((stored_name, definition)) = spec
            .types()
            .as_object()
            .and_then(|object| object.get_key_value(name))
        else {
            continue;
        };
        if let Some(subtypes) = definition["subtypes"].as_array() {
            let subs = subtypes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if let Some(found) = matching_type(spec, value, &subs, seen) {
                return Some(found);
            }
        } else if let Some((key, constant)) = type_const(spec.types(), name) {
            if value[&key].as_str() == Some(constant.as_str()) {
                return Some(stored_name);
            }
        } else if value.is_object()
            && definition["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|field| field["required"] == true)
                .all(|field| value.get(field["name"].as_str().unwrap_or("")).is_some())
        {
            return Some(stored_name);
        }
    }
    None
}

fn hit(rules: &Value, key: &str, path: &str, places: &[(String, String)], out: &mut Vec<String>) {
    let rule = &rules[key];
    if rule.is_null() {
        return;
    }
    if let Some(method) = rule["only_in"].as_str() {
        out.push(format!(
            "{path}: работает только в {method}, не в публикуемом посте"
        ));
    }
    for (kind, label) in places {
        if let Some(reason) = rule["refused"][kind]["text"].as_str() {
            let hint = rule["hint"].as_str().unwrap_or("");
            out.push(format!("{path}: не работает {label}; {reason}. {hint}"));
        }
    }
}

fn walk(
    spec: &Spec,
    rules: &Value,
    value: &Value,
    names: &[String],
    path: &str,
    places: &[(String, String)],
    out: &mut Vec<String>,
) {
    if let Some(items) = value.as_array() {
        let mut inner = names
            .iter()
            .filter_map(|name| name.strip_prefix("Array of ").map(str::to_owned))
            .collect::<Vec<_>>();
        if inner.is_empty() {
            for name in names {
                inner.extend(
                    spec.types()[name]["subtypes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter_map(|name| name.strip_prefix("Array of "))
                        .map(str::to_owned),
                );
            }
        }
        for (i, item) in items.iter().enumerate() {
            walk(
                spec,
                rules,
                item,
                &inner,
                &format!("{path}[{i}]"),
                places,
                out,
            );
        }
    } else if value.is_object()
        && let Some(name) = matching_type(spec, value, names, &mut BTreeSet::new())
    {
        hit(rules, name, path, places, out);
        for field in spec.types()[name]["fields"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let key = field["name"].as_str().unwrap_or("");
            if let Some(child) = value.get(key) {
                let child_path = format!("{path}.{key}");
                hit(rules, &format!("{name}.{key}"), &child_path, places, out);
                walk(spec, rules, child, &types(field), &child_path, places, out);
            }
        }
    }
}

pub fn warnings(spec: &Spec, post: &Value, places: &[(String, String)]) -> Vec<String> {
    if super::validator::json_depth(post) > 200 {
        return Vec::new();
    }
    let summary = summary(spec);
    let rules = &summary["rules"];
    let mut out = Vec::new();
    for field in spec.methods()["sendRichMessage"]["fields"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let key = field["name"].as_str().unwrap_or("");
        if let Some(value) = post
            .get(key)
            .or_else(|| post.get("options").and_then(|options| options.get(key)))
        {
            hit(
                rules,
                &format!("sendRichMessage.{key}"),
                key,
                places,
                &mut out,
            );
            walk(spec, rules, value, &types(field), key, places, &mut out);
        }
    }
    // Custom HTML tags name their block in the specification; their restrictions follow it too.
    let tags =
        Regex::new(r"<([A-Za-z][A-Za-z0-9-]*)[\s>]").unwrap_or_else(|e| panic!("tag regex: {e}"));
    let source = post["rich_message"]["html"]
        .as_str()
        .or_else(|| post["rich_message"]["markdown"].as_str())
        .unwrap_or("");
    let present = tags
        .captures_iter(source)
        .map(|c| c[1].to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    for (name, definition) in spec.types().as_object().into_iter().flatten() {
        let text = description(&definition["description"]);
        if text.contains("corresponding to the")
            && present.iter().any(|tag| text.contains(&format!("<{tag}>")))
        {
            hit(rules, name, "rich_message", places, &mut out);
        }
    }
    for (start, end) in super::validator::text::tags(source) {
        let tag = &source[start..end];
        if tag
            .get(..10)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("<tg-button"))
            && tag
                .as_bytes()
                .get(10)
                .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'>')
        {
            let action = super::validator::text::attribute(tag, "type", true)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    if super::validator::text::attribute(tag, "url", false).is_some() {
                        "url"
                    } else {
                        "callback_data"
                    }
                    .to_owned()
                });
            hit(
                rules,
                &format!("RichMessageButton.{action}"),
                &format!("rich_message: <tg-button type=\"{action}\">"),
                places,
                &mut out,
            );
        }
    }
    out.sort();
    out.dedup();
    out
}
