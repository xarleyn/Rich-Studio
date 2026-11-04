use serde_json::{Value, json};

pub const TRACK_PREFIXES: &[&str] = &[
    "InputRich",
    "RichText",
    "RichBlock",
    "RichMessage",
    "InlineKeyboard",
    "CopyTextButton",
    "DisabledButton",
    "InputMedia",
    "LinkPreview",
];
pub const TRACK_METHODS: &[&str] = &[
    "sendRichMessage",
    "editMessageText",
    "sendMessage",
    "editMessageReplyMarkup",
    "pinChatMessage",
    "deleteMessage",
    "getChatMember",
    "getMe",
];

fn tracked(name: &str) -> bool {
    TRACK_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

fn signatures(definition: &Value) -> Vec<(String, Value)> {
    definition
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|field| {
            Some((
                field.get("name")?.as_str()?.to_owned(),
                field.get("types").cloned().unwrap_or(Value::Null),
            ))
        })
        .collect()
}

fn description(definition: &Value) -> String {
    definition
        .get("description")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect()
}

pub fn diff_specs(old: &Value, new: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    let empty = json!({});
    let old_types = old.get("types").unwrap_or(&empty);
    let new_types = new.get("types").unwrap_or(&empty);
    for (name, definition) in new_types.as_object().into_iter().flatten() {
        let old_definition = old_types.get(name);
        if old_definition.is_none() {
            if tracked(name) || name.contains("Rich") {
                items.push(json!({"kind": "type_added", "name": name, "description": description(definition)}));
            }
            continue;
        }
        if !tracked(name) {
            continue;
        }
        let old_definition = old_definition.unwrap_or(&Value::Null);
        let old_fields = signatures(old_definition);
        let new_fields = signatures(definition);
        for (field, types) in &new_fields {
            if !old_fields.iter().any(|(name, _)| name == field) {
                items.push(json!({"kind": "field_added", "name": format!("{name}.{field}"), "types": types}));
            }
        }
        for (field, _) in &old_fields {
            if !new_fields.iter().any(|(name, _)| name == field) {
                items.push(json!({"kind": "field_removed", "name": format!("{name}.{field}")}));
            }
        }
        let old_subtypes = old_definition
            .get("subtypes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for subtype in definition
            .get("subtypes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !old_subtypes.contains(subtype) {
                items.push(json!({"kind": "subtype_added", "name": format!("{name} → {}", subtype.as_str().unwrap_or_default())}));
            }
        }
    }
    let old_methods = old.get("methods").unwrap_or(&empty);
    for (name, definition) in new
        .get("methods")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        match old_methods.get(name) {
            None => items.push(json!({"kind": "method_added", "name": name, "description": description(definition)})),
            Some(old_definition) if TRACK_METHODS.contains(&name.as_str()) => {
                let old_fields = signatures(old_definition);
                for (field, types) in signatures(definition) {
                    if !old_fields.iter().any(|(name, _)| name == &field) {
                        items.push(json!({"kind": "param_added", "name": format!("{name}({field})"), "types": types}));
                    }
                }
            }
            Some(_) => {}
        }
    }
    items.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    items
}
