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
                match value.as_array() {
                    Some(items) => Value::Array(
                        items
                            .iter()
                            .map(|child| convert_block(spec, kinds, child))
                            .collect(),
                    ),
                    // A malformed value is data, not an empty list: keep it for the error message.
                    None => value.clone(),
                }
            } else if let Some(item) = name.strip_prefix("Array of InputRich") {
                let name = format!("InputRich{item}");
                match value.as_array() {
                    Some(items) => Value::Array(
                        items
                            .iter()
                            .map(|child| convert_object(spec, kinds, child, &name))
                            .collect(),
                    ),
                    None => value.clone(),
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use std::collections::BTreeMap;

    fn spec() -> Spec {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let settings = Settings::from_map(&BTreeMap::from([
            (
                "DATA_DIR".to_owned(),
                directory.path().to_string_lossy().into_owned(),
            ),
            ("SPEC_PIN".to_owned(), "on".to_owned()),
        ]))
        .expect("settings");
        Spec::load_initial(&settings)
    }

    #[test]
    fn sent_media_reuses_the_largest_file_id() {
        let spec = spec();
        let sent = json!([
            {
                "type":"photo",
                "photo":[
                    {"file_id":"small","width":90,"height":60},
                    {"file_id":"big","width":1280,"height":720}
                ],
                "has_spoiler":true,
                "caption":{"text":"Обложка"}
            },
            {
                "type":"list",
                "items":[{"label":"•","blocks":[{"type":"video","video":{"file_id":"v1","duration":3}}]}]
            }
        ]);
        let values = to_input_blocks(&spec, sent.as_array().expect("array"));
        assert_eq!(
            values[0],
            json!({
                "type":"photo",
                "photo":{"type":"photo","media":"big","has_spoiler":true},
                "caption":{"text":"Обложка"}
            })
        );
        assert_eq!(
            values[1]["items"],
            json!([{"blocks":[{"type":"video","video":{"type":"video","media":"v1"}}]}])
        );
    }

    #[test]
    fn unknown_and_malformed_blocks_are_kept_and_do_not_panic() {
        let spec = spec();
        assert_eq!(
            to_input_blocks(&spec, &[json!({"type":"hologram","x":1})]),
            vec![json!({"type":"hologram","x":1})]
        );
        let odd = [
            json!({"type":"blockquote","blocks":null}),
            json!({"type":"blockquote","blocks":"x"}),
            json!({"type":"photo","photo":[1,2]}),
            json!({"type":"video","video":"v"}),
        ];
        let values = to_input_blocks(&spec, &odd);
        assert_eq!(values[0]["blocks"], Value::Null);
        assert_eq!(values[1]["blocks"], "x");
        assert_eq!(values[2]["photo"]["media"], "");
        assert_eq!(values[3]["video"]["media"], "");
    }
}
