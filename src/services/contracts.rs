//! Validate the bounded JSON schema vocabulary used by the saved HTTP/MCP contracts.
//! Payload objects remain open: newer Telegram fields are not rejected here.
use serde_json::{Value, json};
use std::sync::OnceLock;

pub fn http_document() -> Result<&'static Value, &'static str> {
    static DOCUMENT: OnceLock<Result<Value, serde_json::Error>> = OnceLock::new();
    DOCUMENT
        .get_or_init(|| serde_json::from_str(include_str!("../api/compatibility.json")))
        .as_ref()
        .map_err(|_| "invalid HTTP contract")
}

/// Pydantic models coerce their own primitive fields, apply defaults and ignore extra model
/// attributes. Untyped Telegram dictionaries are an explicit boundary and remain unchanged.
pub fn normalize(root: &Value, schema: &Value, value: &Value) -> Result<Value, String> {
    normalize_at(root, schema, value, "body", 0, true)
}

/// HTTP adapters retain omitted fields for patch-like operations. Post is always the complete
/// model_dump shape because previews, publication and idempotency all consume that value.
pub fn normalize_present(root: &Value, schema: &Value, value: &Value) -> Result<Value, String> {
    normalize_at(root, schema, value, "body", 0, false)
}

pub fn parse_post(value: &Value) -> Result<Value, String> {
    let document = http_document()?;
    normalize(document, &document["components"]["schemas"]["Post"], value)
        .map_err(|error| format!("неверный пост: {error}"))
}

pub fn empty_body(root: &Value, schema: &Value) -> Value {
    if let Some(reference) = schema["$ref"].as_str()
        && let Some(target) = reference
            .strip_prefix('#')
            .and_then(|path| root.pointer(path))
    {
        return empty_body(root, target);
    }
    if schema["anyOf"]
        .as_array()
        .is_some_and(|variants| variants.iter().any(|v| v["type"] == "null"))
    {
        Value::Null
    } else if schema["type"] == "object" {
        json!({})
    } else {
        Value::Null
    }
}

fn normalize_at(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    depth: usize,
    defaults: bool,
) -> Result<Value, String> {
    if depth > 64 {
        return Err(format!("{path}: nesting limit exceeded"));
    }
    if let Some(reference) = schema["$ref"].as_str() {
        let target = reference
            .strip_prefix('#')
            .and_then(|p| root.pointer(p))
            .ok_or_else(|| format!("{path}: unresolved schema"))?;
        return normalize_at(root, target, value, path, depth + 1, defaults);
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        // Pydantic's smart union keeps an already matching string even if integer appears first.
        for variant in variants {
            if check(root, variant, value, path, depth + 1).is_ok() {
                return normalize_at(root, variant, value, path, depth + 1, defaults);
            }
        }
        for variant in variants {
            if let Ok(value) = normalize_at(root, variant, value, path, depth + 1, defaults) {
                return Ok(value);
            }
        }
        return Err(format!("{path}: unexpected value type or range"));
    }
    let mut normalized = match schema["type"].as_str() {
        Some("boolean") => match value {
            Value::String(value) => match value.to_ascii_lowercase().as_str() {
                "1" | "on" | "t" | "true" | "y" | "yes" => json!(true),
                "0" | "off" | "f" | "false" | "n" | "no" => json!(false),
                _ => value.clone().into(),
            },
            Value::Number(number) if number.as_f64() == Some(1.0) => json!(true),
            Value::Number(number) if number.as_f64() == Some(0.0) => json!(false),
            _ => value.clone(),
        },
        Some("integer") => coerce_integer(value).unwrap_or_else(|| value.clone()),
        Some("number") => {
            let number = match value {
                Value::String(value) => value.trim().parse::<f64>().ok(),
                Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
                Value::Number(number) => number.as_f64(),
                _ => None,
            };
            number
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .unwrap_or_else(|| value.clone())
        }
        _ => value.clone(),
    };
    if let Value::Object(fields) = &mut normalized {
        if let Some(properties) = schema["properties"].as_object() {
            if schema["additionalProperties"] == false {
                if let Some(name) = fields.keys().find(|name| !properties.contains_key(*name)) {
                    return Err(format!("{path}.{name}: unknown argument"));
                }
            } else if schema.get("additionalProperties").is_none() {
                fields.retain(|name, _| properties.contains_key(name));
            }
            for (name, child) in properties {
                if let Some(value) = fields.get_mut(name) {
                    // strict=True is not represented in the exported JSON Schema.
                    if schema["title"] == "TokenReq" && name == "expires_in_days" {
                        check(root, child, value, &format!("{path}.{name}"), depth + 1)?;
                    } else {
                        *value = normalize_at(
                            root,
                            child,
                            value,
                            &format!("{path}.{name}"),
                            depth + 1,
                            defaults,
                        )?;
                    }
                } else if (defaults || schema["title"] == "Post")
                    && !schema["required"].as_array().is_some_and(|required| {
                        required.iter().any(|key| key.as_str() == Some(name))
                    })
                {
                    if let Some(default) = child.get("default") {
                        fields.insert(name.clone(), default.clone());
                    } else if child["anyOf"]
                        .as_array()
                        .is_some_and(|variants| variants.iter().any(|v| v["type"] == "null"))
                    {
                        fields.insert(name.clone(), Value::Null);
                    } else if child["type"] == "array" {
                        fields.insert(name.clone(), json!([]));
                    } else if child["type"] == "object" {
                        fields.insert(name.clone(), json!({}));
                    }
                }
            }
        }
        if let Some(child) = schema
            .get("additionalProperties")
            .filter(|child| child.is_object())
        {
            for (name, value) in fields {
                *value = normalize_at(
                    root,
                    child,
                    value,
                    &format!("{path}.{name}"),
                    depth + 1,
                    defaults,
                )?;
            }
        }
    } else if let Value::Array(items) = &mut normalized
        && let Some(child) = schema.get("items")
    {
        for (index, item) in items.iter_mut().enumerate() {
            *item = normalize_at(
                root,
                child,
                item,
                &format!("{path}[{index}]"),
                depth + 1,
                defaults,
            )?;
        }
    }
    // SQLite identifiers/versions are i64. Never let a present oversized integer become None
    // in an adapter's as_i64(), which would turn a versioned update into an unconditional one.
    if schema["type"] == "integer" && normalized.is_number() && normalized.as_i64().is_none() {
        return Err(format!("{path}: outside supported integer range"));
    }
    check(root, schema, &normalized, path, depth)?;
    Ok(normalized)
}

fn coerce_integer(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(value) => Some(json!(i64::from(*value))),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(value.clone()),
        Value::Number(number) => number
            .as_f64()
            .filter(|n| n.fract() == 0.0 && *n >= i64::MIN as f64 && *n < i64::MAX as f64)
            .map(|n| json!(n as i64)),
        Value::String(text) => {
            let text = text.trim();
            let integer = if let Some((integer, fraction)) = text.split_once('.') {
                if fraction.is_empty() || !fraction.bytes().all(|ch| ch == b'0') {
                    return None;
                }
                integer
            } else {
                text
            };
            integer.parse::<i64>().map(|n| json!(n)).ok()
        }
        _ => None,
    }
}

pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, schema, value, "arguments", 0)
}
pub fn validate_in(root: &Value, schema: &Value, value: &Value) -> Result<(), String> {
    check(root, schema, value, "body", 0)
}
fn check(
    root: &Value,
    schema: &Value,
    value: &Value,
    path: &str,
    depth: usize,
) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("{path}: nesting limit exceeded"));
    }
    if let Some(reference) = schema["$ref"].as_str() {
        let target = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
            .ok_or_else(|| format!("{path}: unresolved schema"))?;
        return check(root, target, value, path, depth + 1);
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        return if variants
            .iter()
            .any(|variant| check(root, variant, value, path, depth + 1).is_ok())
        {
            Ok(())
        } else {
            Err(format!("{path}: unexpected value type or range"))
        };
    }
    if let Some(variants) = schema["allOf"].as_array() {
        for variant in variants {
            check(root, variant, value, path, depth + 1)?;
        }
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(value)
    {
        return Err(format!("{path}: value is not allowed"));
    }
    if let Some(expected) = schema.get("const")
        && value != expected
    {
        return Err(format!("{path}: unexpected constant"));
    }
    if let Some(kind) = schema["type"].as_str() {
        let valid = match kind {
            "null" => value.is_null(),
            "boolean" => value.is_boolean(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            _ => false,
        };
        if !valid {
            return Err(format!("{path}: expected {kind}"));
        }
    }
    match value {
        Value::Object(fields) => {
            if let Some(required) = schema["required"].as_array() {
                for name in required.iter().filter_map(Value::as_str) {
                    if !fields.contains_key(name) {
                        return Err(format!("{path}.{name}: required"));
                    }
                }
            }
            for (name, value) in fields {
                if let Some(child) = schema["properties"].get(name) {
                    check(root, child, value, &format!("{path}.{name}"), depth + 1)?;
                } else if schema["additionalProperties"] == false {
                    return Err(format!("{path}.{name}: unknown argument"));
                } else if schema["additionalProperties"].is_object() {
                    check(
                        root,
                        &schema["additionalProperties"],
                        value,
                        &format!("{path}.{name}"),
                        depth + 1,
                    )?;
                }
            }
        }
        Value::Array(items) => {
            bound(schema, path, items.len() as f64, "minItems", "maxItems")?;
            if let Some(child) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check(root, child, item, &format!("{path}[{index}]"), depth + 1)?;
                }
            }
        }
        Value::String(text) => {
            bound(
                schema,
                path,
                text.chars().count() as f64,
                "minLength",
                "maxLength",
            )?;
            if let Some(pattern) = schema["pattern"].as_str() {
                let regex = regex::Regex::new(pattern)
                    .map_err(|_| format!("{path}: invalid schema pattern"))?;
                if !regex.is_match(text) {
                    return Err(format!("{path}: invalid format"));
                }
            }
        }
        Value::Number(number) => {
            if let Some(number) = number.as_f64() {
                bound(schema, path, number, "minimum", "maximum")?;
                if schema["exclusiveMinimum"]
                    .as_f64()
                    .is_some_and(|min| number <= min)
                    || schema["exclusiveMaximum"]
                        .as_f64()
                        .is_some_and(|max| number >= max)
                {
                    return Err(format!("{path}: outside allowed range"));
                }
            }
        }
        _ => {}
    }
    Ok(())
}
fn bound(schema: &Value, path: &str, value: f64, min: &str, max: &str) -> Result<(), String> {
    if schema[min].as_f64().is_some_and(|min| value < min)
        || schema[max].as_f64().is_some_and(|max| value > max)
    {
        return Err(format!("{path}: outside allowed range"));
    }
    Ok(())
}
