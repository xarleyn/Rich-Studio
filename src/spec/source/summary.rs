use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

use super::Spec;
use crate::spec::diff::{TRACK_METHODS, TRACK_PREFIXES};
use crate::spec::{enum_values, type_const};

const LABELS: &str = include_str!("../../../app/static/js/labels.js");
const RENDER: &str = include_str!("../../../app/static/render.js");

impl Spec {
    pub fn summary(&self) -> Value {
        let mut output_types = Map::new();
        let mut reachable = BTreeSet::new();
        let mut pending = TRACK_METHODS
            .iter()
            .flat_map(|name| {
                self.methods()[*name]["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .flat_map(|field| field["types"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        while let Some(name) = pending.pop() {
            let name = name.strip_prefix("Array of ").unwrap_or(&name).to_owned();
            if name.starts_with("Array of ") {
                pending.push(name);
                continue;
            }
            if !reachable.insert(name.clone()) {
                continue;
            }
            let definition = &self.types()[&name];
            pending.extend(
                definition["subtypes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            pending.extend(
                definition["fields"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|field| field["types"].as_array().into_iter().flatten())
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
        }
        for (name, definition) in self.types().as_object().into_iter().flatten() {
            if !reachable.contains(name)
                && !TRACK_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
                && ![
                    "Location",
                    "LoginUrl",
                    "SwitchInlineQueryChosenChat",
                    "WebAppInfo",
                    "User",
                    "RichBlockTableCell",
                    "RichBlockCaption",
                ]
                .contains(&name.as_str())
            {
                continue;
            }
            let fields = summary_fields(definition, true);
            let constant = type_const(self.types(), name);
            output_types.insert(
                name.clone(),
                json!({
                    "description": description(definition),
                    "fields": fields,
                    "subtypes": definition.get("subtypes").cloned().unwrap_or_else(|| json!([])),
                    "const": constant.as_ref().map(|(_, value)| value),
                    "const_field": constant.as_ref().map(|(field, _)| field),
                    "href": definition.get("href").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        let mut methods = Map::new();
        for name in TRACK_METHODS {
            if let Some(definition) = self.methods().get(name) {
                methods.insert(
                    (*name).to_owned(),
                    json!({
                        "description": description(definition),
                        "fields": summary_fields(definition, false),
                        "href": definition.get("href").cloned().unwrap_or(Value::Null),
                    }),
                );
            }
        }
        json!({
            "version": self.version(),
            "release_date": self.data.get("release_date").cloned().unwrap_or(Value::Null),
            "changelog": self.data.get("changelog").cloned().unwrap_or(Value::Null),
            "source": self.source,
            "loaded_at": self.loaded_at as i64,
            "types": output_types,
            "methods": methods,
            "changes": self.changes.iter().take(10).cloned().collect::<Vec<_>>(),
            "gaps": gaps(self.types()),
            "last_check": self.last_check,
            "chat_rules": crate::spec::chats::summary(self),
        })
    }
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
}

fn summary_fields(definition: &Value, _enums: bool) -> Vec<Value> {
    definition
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|field| {
            let mut output = json!({
                "name": field.get("name").cloned().unwrap_or(Value::Null),
                "types": field.get("types").cloned().unwrap_or_else(|| json!([])),
                "required": field.get("required").and_then(Value::as_bool).unwrap_or(false),
                "description": field.get("description").cloned().unwrap_or_else(|| json!("")),
            });
            {
                output["enum"] = field
                    .get("description")
                    .and_then(Value::as_str)
                    .and_then(enum_values)
                    .map_or(Value::Null, |values| json!(values));
            }
            if let Some((minimum, maximum)) = crate::spec::validator::number_range(field) {
                output["minimum"] = json!(minimum);
                output["maximum"] = json!(maximum);
                output["range"] = json!([minimum, maximum]);
            }
            output
        })
        .collect()
}

pub(super) fn consts(types: &Value, base: &str) -> Vec<(String, String)> {
    types
        .get(base)
        .and_then(|definition| definition.get("subtypes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|subtype| {
            let name = subtype.as_str()?;
            let (_, constant) = type_const(types, name)?;
            Some((constant, name.to_owned()))
        })
        .collect()
}

fn object_keys(source: &str, variable: &str) -> Vec<String> {
    let pattern = Regex::new(&format!(r"\b{variable}\s*=\s*\{{([\s\S]*?)\}};"))
        .unwrap_or_else(|error| panic!("JS regex: {error}"));
    let Some(body) = pattern
        .captures(source)
        .and_then(|found| found.get(1))
        .map(|body| body.as_str())
    else {
        return Vec::new();
    };
    Regex::new(r"(?:^|[,{\s])([a-z_]+)\s*:")
        .unwrap_or_else(|error| panic!("JS key regex: {error}"))
        .captures_iter(body)
        .map(|key| key[1].to_owned())
        .collect()
}

fn function_cases(source: &str, function: &str, inline: bool) -> Vec<String> {
    let Some(start) = source.find(&format!("function {function}(")) else {
        return Vec::new();
    };
    let tail = &source[start..];
    let end = tail[1..]
        .find("\n  function ")
        .map_or(tail.len(), |end| end + 1);
    let body = &tail[..end];
    let pattern = if inline {
        r"'([a-z_]+)'"
    } else {
        r"case '([a-z_]+)'"
    };
    Regex::new(pattern)
        .unwrap_or_else(|error| panic!("JS case regex: {error}"))
        .captures_iter(body)
        .map(|key| key[1].to_owned())
        .collect()
}

pub(super) fn gaps(types: &Value) -> Vec<Value> {
    let labels = LABELS;
    let render = RENDER;
    let mut output = Vec::new();
    for (part, file, base, kind) in [
        ("labels", "js/labels.js", "InputRichBlock", "block"),
        ("labels", "js/labels.js", "RichText", "inline"),
        ("preview", "render.js", "InputRichBlock", "block"),
        ("preview", "render.js", "RichText", "inline"),
    ] {
        let (table, function, wraps) = match (part, base) {
            ("labels", "InputRichBlock") => ("BLOCK_LABELS", "", false),
            ("labels", _) => ("RT_LABELS", "", false),
            (_, "InputRichBlock") => ("", "renderBlock", false),
            _ => ("", "renderRT", true),
        };
        let source = if part == "labels" { labels } else { render };
        let mut known = if table.is_empty() {
            function_cases(source, function, kind == "inline")
        } else {
            object_keys(source, table)
        };
        if wraps {
            known.extend(object_keys(source, "WRAP"));
        }
        let where_ = if table.is_empty() { function } else { table };
        for (constant, type_name) in consts(types, base) {
            if !known.contains(&constant) {
                output.push(json!({"part": part, "file": file, "where": where_, "kind": kind, "name": constant, "type": type_name}));
            }
        }
    }
    output.sort_by(|left, right| {
        format!(
            "{}{}{}",
            left["part"].as_str().unwrap_or_default(),
            left["kind"].as_str().unwrap_or_default(),
            left["name"].as_str().unwrap_or_default()
        )
        .cmp(&format!(
            "{}{}{}",
            right["part"].as_str().unwrap_or_default(),
            right["kind"].as_str().unwrap_or_default(),
            right["name"].as_str().unwrap_or_default()
        ))
    });
    output
}
