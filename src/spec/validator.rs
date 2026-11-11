use regex::Regex;
use serde_json::{Map, Value, json};
use url::Url;

use super::source::Spec;
use super::{enum_values, type_const};

pub type Validation = Value;

pub struct Validator<'a> {
    spec: &'a Spec,
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl<'a> Validator<'a> {
    pub fn new(spec: &'a Spec) -> Self {
        Self {
            spec,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn check(&self, post: &Value) -> Validation {
        self.check_post(post)
    }

    pub fn check_post(&self, post: &Value) -> Validation {
        let mut validator = Self::new(self.spec);
        let rich_message = post.get("rich_message").unwrap_or(&Value::Null);
        let mut report = validator.validate_rich_message(rich_message);
        let button_errors = keyboard_errors(post.get("reply_markup").unwrap_or(&Value::Null));
        if !button_errors.is_empty() {
            report["ok"] = json!(false);
            report["errors"] = json!(
                report["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .chain(button_errors)
                    .collect::<Vec<_>>()
            );
        }
        report
    }

    fn check_value(&mut self, value: &Value, types: &[String], path: &str) {
        if types.is_empty() {
            return;
        }
        if types.len() == 1 {
            self.check_one(value, &types[0], path);
            return;
        }
        let mut best: Option<Self> = None;
        for type_name in types {
            let mut candidate = Self::new(self.spec);
            candidate.check_one(value, type_name, path);
            if candidate.errors.is_empty() {
                self.warnings.extend(candidate.warnings);
                return;
            }
            if best
                .as_ref()
                .is_none_or(|current| candidate.errors.len() < current.errors.len())
            {
                best = Some(candidate);
            }
        }
        if let Some(best) = best {
            self.errors.push(format!(
                "{path}: не подходит ни под один из типов {} (ближайший: {})",
                types.join(", "),
                best.errors.first().cloned().unwrap_or_default()
            ));
        }
    }

    fn check_one(&mut self, value: &Value, type_name: &str, path: &str) {
        if let Some(inner) = type_name.strip_prefix("Array of ") {
            let Some(items) = value.as_array() else {
                self.errors
                    .push(format!("{path}: ожидался массив ({type_name})"));
                return;
            };
            for (index, item) in items.iter().enumerate() {
                self.check_one(item, inner, &format!("{path}[{index}]"));
            }
            return;
        }
        match type_name {
            "String" | "InputFile" => {
                if !value.is_string() {
                    self.errors.push(format!("{path}: ожидалась строка"));
                }
                return;
            }
            "Integer" => {
                if !value.is_i64() && !value.is_u64() {
                    self.errors.push(format!("{path}: ожидалось целое число"));
                }
                return;
            }
            "Float" => {
                if !value.is_number() {
                    self.errors.push(format!("{path}: ожидалось число"));
                }
                return;
            }
            "Boolean" | "True" => {
                if !value.is_boolean() {
                    self.errors.push(format!("{path}: ожидалось true/false"));
                }
                return;
            }
            _ => {}
        }
        let Some(definition) = self.spec.types().get(type_name) else {
            self.warnings.push(format!(
                "{path}: тип {type_name} не найден в спецификации, не проверяю"
            ));
            return;
        };
        let subtypes = definition
            .get("subtypes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !subtypes.is_empty() {
            if value.is_object() {
                let candidates: Vec<String> = subtypes
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|name| self.spec.types().get(name).is_some())
                    .map(str::to_owned)
                    .collect();
                let tagged: Vec<String> = candidates
                    .iter()
                    .filter(|name| {
                        type_const(self.spec.types(), name).is_some_and(|(field, constant)| {
                            value.get(&field).and_then(Value::as_str) == Some(constant.as_str())
                        })
                    })
                    .cloned()
                    .collect();
                if !tagged.is_empty() {
                    self.check_value(value, &tagged, path);
                    return;
                }
                let untagged: Vec<String> = candidates
                    .iter()
                    .filter(|name| type_const(self.spec.types(), name).is_none())
                    .cloned()
                    .collect();
                if !untagged.is_empty() {
                    self.check_value(value, &untagged, path);
                    return;
                }
                self.errors.push(format!(
                    "{path}: неизвестный type={} для {type_name}",
                    python_repr(value.get("type").unwrap_or(&Value::Null))
                ));
                return;
            }
            self.check_value(value, &candidates(subtypes), path);
            return;
        }
        let Some(object) = value.as_object() else {
            self.errors
                .push(format!("{path}: ожидался объект {type_name}"));
            return;
        };
        let fields = definition
            .get("fields")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for field in &fields {
            let name = field
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if field
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !object.contains_key(name)
            {
                self.errors.push(format!(
                    "{path}: нет обязательного поля {name} ({type_name})"
                ));
            }
        }
        for (name, item) in object {
            let Some(field) = fields
                .iter()
                .find(|field| field.get("name").and_then(Value::as_str) == Some(name.as_str()))
            else {
                self.warnings.push(format!(
                    "{path}.{name}: поля нет в {type_name} (спецификация {})",
                    self.spec.version()
                ));
                continue;
            };
            if let Some((constant_name, constant)) = type_const(self.spec.types(), type_name)
                && constant_name == *name
                && item.as_str() != Some(constant.as_str())
            {
                self.errors.push(format!(
                    "{path}.{name}: должно быть '{}', а не {}",
                    constant,
                    python_repr(item)
                ));
                continue;
            }
            if field
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && field
                    .get("types")
                    .and_then(Value::as_array)
                    .is_some_and(|types| types.len() == 1 && types[0] == "RichText")
                && (item == "" || item == &json!([]))
            {
                self.warnings.push(format!(
                    "{path}.{name}: пустой текст, Telegram может отклонить блок"
                ));
            }
            if name == "url"
                && let Some(problem) = url_error(item)
            {
                self.warnings.push(format!("{path}.{name}: {problem}"));
            }
            if let Some(allowed) = field
                .get("description")
                .and_then(Value::as_str)
                .and_then(enum_values)
                && allowed.len() > 1
                && item
                    .as_str()
                    .is_some_and(|actual| !allowed.iter().any(|allowed| allowed == actual))
            {
                self.warnings.push(format!(
                    "{path}.{name}: значение {item}, ожидалось одно из {allowed:?}"
                ));
            }
            let types = field
                .get("types")
                .and_then(Value::as_array)
                .map(|types| {
                    types
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.check_value(item, &types, &format!("{path}.{name}"));
        }
    }

    fn validate_rich_message(&mut self, rich_message: &Value) -> Validation {
        if !rich_message.is_object() {
            return json!({"ok": false, "errors": ["rich_message должен быть объектом"], "warnings": [], "stats": {}});
        }
        if json_depth(rich_message) > 200 {
            return json!({"ok": false, "errors": ["вложенность больше 200 уровней"], "warnings": [], "stats": {}});
        }
        let present: Vec<&str> = ["html", "markdown", "blocks"]
            .into_iter()
            .filter(|field| {
                rich_message
                    .get(field)
                    .is_some_and(|value| !value.is_null() && value != "" && value != &json!([]))
            })
            .collect();
        if present.len() != 1 {
            self.errors.push(format!(
                "нужно ровно одно из html / markdown / blocks, сейчас: {}",
                if present.is_empty() {
                    "ничего".to_owned()
                } else {
                    present.join(", ")
                }
            ));
        }
        self.check_value(
            rich_message,
            &["InputRichMessage".to_owned()],
            "rich_message",
        );
        let mut stats = Map::new();
        if let Some(blocks) = rich_message.get("blocks").and_then(Value::as_array) {
            let walked = walk_blocks(blocks, 1);
            stats.insert("blocks".to_owned(), json!(walked.0));
            stats.insert("depth".to_owned(), json!(walked.1));
            stats.insert("chars".to_owned(), json!(walked.2));
        } else {
            let source = rich_message
                .get("html")
                .or_else(|| rich_message.get("markdown"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let tags = Regex::new(r"<[^>]+>").unwrap_or_else(|error| panic!("HTML regex: {error}"));
            stats.insert(
                "chars".to_owned(),
                json!(tags.replace_all(source, "").chars().count()),
            );
            let references = Regex::new(
                r"tg://(photo|video|audio|document|animation|voice(?:_note)?)\?id=([\w\-.]+)",
            )
            .unwrap_or_else(|error| panic!("media regex: {error}"));
            let media: Vec<&str> = rich_message
                .get("media")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|item| item.get("id"))
                .filter_map(Value::as_str)
                .collect();
            for found in references.captures_iter(source) {
                if !media.contains(&found.get(2).map_or("", |id| id.as_str())) {
                    self.errors.push(format!(
                        "в тексте есть {}, но в media нет элемента с id='{}'",
                        found.get(0).map_or("", |found| found.as_str()),
                        found.get(2).map_or("", |id| id.as_str())
                    ));
                }
            }
        }
        let media_count = rich_message
            .get("media")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        stats.insert("media".to_owned(), json!(media_count));
        let chars = stats.get("chars").and_then(Value::as_i64).unwrap_or(0);
        if chars > 32768 {
            self.errors
                .push(format!("текст {chars} символов, лимит 32768"));
        } else if chars > 8000 {
            self.warnings
                .push("больше ~8000 символов: клиент свернёт пост под «Показать ещё»".to_owned());
        }
        if media_count > 50 {
            self.warnings.push(format!(
                "{media_count} медиа, по неофициальным данным лимит 50"
            ));
        }
        json!({"ok": self.errors.is_empty(), "errors": self.errors, "warnings": self.warnings, "stats": stats, "spec_version": self.spec.version()})
    }
}

fn candidates(subtypes: Vec<Value>) -> Vec<String> {
    subtypes
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn url_error(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    if raw.trim().is_empty() {
        return Some("пустая ссылка".to_owned());
    }
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed).ok();
    if trimmed.to_lowercase().starts_with("http") {
        let authority = trimmed
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        if authority.is_empty()
            || authority.starts_with('/')
            || parsed.as_ref().and_then(Url::host_str).is_none()
        {
            return Some(format!("в ссылке {raw:?} нет адреса сайта"));
        }
    }
    None
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => format!("'{text}'"),
        other => other.to_string(),
    }
}

pub fn keyboard_errors(markup: &Value) -> Vec<String> {
    let mut errors = Vec::new();
    for (row_index, row) in markup
        .get("inline_keyboard")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (button_index, button) in row.as_array().into_iter().flatten().enumerate() {
            let Some(object) = button.as_object() else {
                continue;
            };
            let where_ = format!(
                "кнопка «{}» (ряд {}, №{})",
                button
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                row_index + 1,
                button_index + 1
            );
            let action = object.keys().any(|key| {
                !["text", "icon_custom_emoji_id", "style", "disabled"].contains(&key.as_str())
            });
            if !action {
                errors.push(format!(
                    "{where_}: не задано, что она делает (ссылка, ответ на нажатие, копирование…)"
                ));
            } else if let Some(problem) = button.get("url").and_then(url_error) {
                let problem = if problem == "пустая ссылка" {
                    format!(
                        "{problem}: впишите адрес или выберите кнопке другой тип (уберите поле url)"
                    )
                } else {
                    problem
                };
                errors.push(format!("{where_}: {problem}"));
            }
        }
    }
    errors
}

fn json_depth(value: &Value) -> usize {
    let mut depth: usize = 0;
    let mut level = vec![value];
    while !level.is_empty() && depth <= 201 {
        depth += 1;
        level = level
            .into_iter()
            .flat_map(|item| match item {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                Value::Object(items) => items.values().collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
    }
    depth.saturating_sub(1)
}

fn rich_text_len(value: &Value) -> usize {
    match value {
        Value::String(text) => text.chars().count(),
        Value::Array(items) => items.iter().map(rich_text_len).sum(),
        Value::Object(item) => item.get("text").map_or(0, rich_text_len),
        _ => 0,
    }
}

fn walk_blocks(blocks: &[Value], depth: usize) -> (usize, usize, usize) {
    let mut stats = (0, depth, 0);
    for block in blocks.iter().filter_map(Value::as_object) {
        stats.0 += 1;
        for key in ["text", "summary", "caption", "credit"] {
            if let Some(value) = block.get(key) {
                stats.2 += rich_text_len(value);
            }
        }
        if let Some(children) = block.get("blocks").and_then(Value::as_array) {
            let nested = walk_blocks(children, depth + 1);
            stats.0 += nested.0;
            stats.1 = stats.1.max(nested.1);
            stats.2 += nested.2;
        }
    }
    stats
}
