//! Convert Telegram output blocks back to input blocks using the current specification.

use super::{source::Spec, type_const};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub fn to_input_blocks(spec: &Spec, blocks: &[Value]) -> Vec<Value> {
    let kinds = spec.types()["InputRichBlock"]["subtypes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|name| {
            type_const(spec.types(), name).map(|(_, value)| (value, name.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    blocks
        .iter()
        .map(|block| convert_block(spec, &kinds, block))
        .collect()
}

fn convert_block(spec: &Spec, kinds: &BTreeMap<String, String>, value: &Value) -> Value {
    match value
        .get("type")
        .and_then(Value::as_str)
        .and_then(|kind| kinds.get(kind))
    {
        Some(name) => convert_object(spec, kinds, value, name),
        None => value.clone(),
    }
}

fn convert_object(
    spec: &Spec,
    kinds: &BTreeMap<String, String>,
    value: &Value,
    type_name: &str,
) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut output = Map::new();
    for (key, value) in object {
        if let Some(field) = spec.types()[type_name]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|field| field["name"].as_str() == Some(key))
        {
            let types = field["types"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let name = if types.len() == 1 {
                types[0].as_str().unwrap_or("")
            } else {
                ""
            };
            let value = if name == "Array of InputRichBlock" {
                Value::Array(
                    value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|child| convert_block(spec, kinds, child))
                        .collect(),
                )
            } else if let Some(item) = name.strip_prefix("Array of InputRich") {
                let name = format!("InputRich{item}");
                Value::Array(
                    value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|child| convert_object(spec, kinds, child, &name))
                        .collect(),
                )
            } else if name.starts_with("InputMedia") {
                convert_media(spec, name, value, &Value::Object(object.clone()))
            } else {
                value.clone()
            };
            output.insert(key.clone(), value);
        }
    }
    Value::Object(output)
}

fn convert_media(spec: &Spec, name: &str, sent: &Value, parent: &Value) -> Value {
    let media = sent
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .max_by_key(|item| {
                    (
                        item["width"]
                            .as_u64()
                            .unwrap_or(0)
                            .saturating_mul(item["height"].as_u64().unwrap_or(0)),
                        item["file_size"].as_u64().unwrap_or(0),
                    )
                })
        })
        .unwrap_or(sent);
    let kind = type_const(spec.types(), name)
        .map(|(_, kind)| json!(kind))
        .unwrap_or_else(|| parent["type"].clone());
    let mut output = json!({"type":kind,"media":media["file_id"].as_str().unwrap_or("")});
    if parent["has_spoiler"].as_bool() == Some(true)
        && spec.types()[name]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|field| field["name"] == "has_spoiler")
    {
        output["has_spoiler"] = json!(true);
    }
    output
}
