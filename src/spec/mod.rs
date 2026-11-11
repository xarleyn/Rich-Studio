pub mod diff;
pub mod source;
pub mod validator;

use regex::Regex;
use serde_json::Value;

pub fn version_tuple(version: &str) -> Vec<u64> {
    let digits = Regex::new(r"\d+").unwrap_or_else(|error| panic!("version regex: {error}"));
    digits
        .find_iter(version)
        .filter_map(|part| part.as_str().parse().ok())
        .collect()
}

pub fn check_spec_shape(data: &Value) -> Option<String> {
    let Some(object) = data.as_object() else {
        return Some("не объект".to_owned());
    };
    for field in ["version", "types", "methods"] {
        if !object.contains_key(field) {
            return Some(format!("нет поля {field}"));
        }
    }
    if object["types"].get("InputRichBlock").is_none()
        || object["methods"].get("sendRichMessage").is_none()
    {
        return Some("нет типов rich-сообщений".to_owned());
    }
    None
}

pub fn type_const(types: &Value, type_name: &str) -> Option<(String, String)> {
    let fields = types.get(type_name)?.get("fields")?.as_array()?;
    let always = Regex::new(r#"always\s+[“"]([^”"]+)[”"]"#)
        .unwrap_or_else(|error| panic!("constant regex: {error}"));
    let must =
        Regex::new(r"must be ([a-z_]+)$").unwrap_or_else(|error| panic!("constant regex: {error}"));
    for field in fields {
        let name = field.get("name")?.as_str()?;
        if !["type", "source", "status"].contains(&name) {
            continue;
        }
        let description = field
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(found) = always.captures(description) {
            return Some((name.to_owned(), found[1].to_owned()));
        }
        if let Some(found) = must.captures(description) {
            return Some((name.to_owned(), found[1].to_owned()));
        }
    }
    None
}

pub fn enum_values(description: &str) -> Option<Vec<String>> {
    let start = Regex::new(r"(?:must be one of|one of|either)\s+(.*)")
        .unwrap_or_else(|error| panic!("enum regex: {error}"));
    let tail = start.captures(description)?.get(1)?.as_str();
    let tail = tail.split(". ").next().unwrap_or(tail);
    let quoted =
        Regex::new(r#"[“"]([^”"]+)[”"]"#).unwrap_or_else(|error| panic!("enum regex: {error}"));
    let values: Vec<String> = quoted
        .captures_iter(tail)
        .map(|value| value[1].to_owned())
        .collect();
    (!values.is_empty()).then_some(values)
}
pub mod convert;
pub mod import_content;
