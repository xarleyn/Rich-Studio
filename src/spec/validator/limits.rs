use serde_json::{Map, Value, json};

use super::text::address;
use crate::spec::{source::Spec, type_const};

mod html;
pub(super) use html::source;

struct Count<'a> {
    spec: &'a Spec,
    blocks: usize,
    chars: usize,
    longest: usize,
    depth: usize,
    about: usize,
    media: usize,
    errors: &'a mut Vec<String>,
    warnings: &'a mut Vec<String>,
}

impl Count<'_> {
    fn string(&mut self, value: &Value, path: &str) {
        if let Some(text) = value.as_str() {
            self.chars += text.chars().count();
            self.longest = self.longest.max(text.len());
            if text.len() > 34990 {
                self.errors.push(format!("{path}: строка текста {} байт в UTF-8: Telegram молча обрежет хвост после ~35000 байт; разбейте её на несколько абзацев",text.len()));
            }
        }
    }
    fn tagged(&self, union: &str, value: &Value) -> Option<String> {
        self.spec.types()[union]["subtypes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find(|name| {
                type_const(self.spec.types(), name)
                    .is_some_and(|(key, constant)| value[&key] == constant)
            })
            .map(str::to_owned)
    }
    fn blocks(&mut self, items: &[Value], level: usize, sure: usize, path: &str) {
        for (index, item) in items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_object())
        {
            self.blocks += 1;
            let measured = usize::from(matches!(
                item["type"].as_str(),
                Some("paragraph" | "blockquote" | "list" | "details" | "table")
            ));
            self.about = self.about.max(level + 1);
            self.depth = self.depth.max(sure + measured);
            self.fields(
                item,
                self.tagged("InputRichBlock", item).as_deref(),
                level + 1,
                sure + measured,
                &format!("{path}[{index}]"),
            );
        }
    }
    fn text(&mut self, value: &Value, level: usize, sure: usize, path: &str) {
        match value {
            Value::String(_) => self.string(value, path),
            Value::Array(items) => {
                for item in items {
                    self.text(item, level, sure, path);
                }
            }
            Value::Object(_) => {
                let measured = usize::from(matches!(
                    value["type"].as_str(),
                    Some("bold" | "italic" | "underline" | "strikethrough")
                ));
                self.about = self.about.max(level + 1);
                self.depth = self.depth.max(sure + measured);
                self.fields(
                    value,
                    self.tagged("RichText", value).as_deref(),
                    level + 1,
                    sure + measured,
                    path,
                );
            }
            _ => {}
        }
    }
    fn object(&mut self, value: &Value, name: &str, level: usize, sure: usize, path: &str) {
        if self.spec.types()[name]["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|field| {
                field["types"]
                    .as_array()
                    .is_some_and(|types| types.contains(&json!("Array of InputRichBlock")))
            })
        {
            self.blocks += 1;
        }
        self.fields(value, Some(name), level, sure, path);
    }
    fn field(
        &mut self,
        value: &Value,
        name: &str,
        types: &[Value],
        level: usize,
        sure: usize,
        path: &str,
    ) {
        let one = if types.len() == 1 {
            types[0].as_str().unwrap_or("")
        } else {
            ""
        };
        if types.contains(&json!("RichText")) {
            self.text(value, level, sure, path);
        } else if types.contains(&json!("Array of InputRichBlock")) {
            if let Some(items) = value.as_array() {
                self.blocks(items, level, sure, path);
            }
        } else if types
            .iter()
            .filter_map(Value::as_str)
            .any(|name| name.starts_with("InputMedia"))
        {
            self.media += 1;
        } else if ["alternative_text", "expression"].contains(&name) {
            self.string(value, path);
            if name == "expression"
                && let Some(expression) = value.as_str()
            {
                formula(expression, self.warnings, path);
            }
        } else if let Some(inner) = one.strip_prefix("Array of Array of ") {
            if let Some(rows) = value.as_array() {
                for (ri, row) in rows.iter().enumerate() {
                    if let Some(cells) = row.as_array() {
                        self.blocks += 1;
                        let columns = cells
                            .iter()
                            .map(|cell| cell["colspan"].as_u64().unwrap_or(1).max(1))
                            .fold(0u64, u64::saturating_add);
                        if columns > 20 {
                            self.errors.push(format!(
                                "{path}[{ri}]: колонок в таблице {columns}, лимит 20"
                            ));
                        }
                        for (ci, cell) in cells.iter().enumerate() {
                            self.object(cell, inner, level, sure, &format!("{path}[{ri}][{ci}]"));
                        }
                    }
                }
            }
        } else if let Some(inner) = one.strip_prefix("Array of ") {
            if let Some(items) = value.as_array() {
                for (index, item) in items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.is_object())
                {
                    self.object(item, inner, level, sure, &format!("{path}[{index}]"));
                }
            }
        } else if !one.is_empty() && value.is_object() {
            self.object(value, one, level, sure, path);
        }
    }
    fn fields(&mut self, value: &Value, name: Option<&str>, level: usize, sure: usize, path: &str) {
        if value.get("text").is_some() {
            if value["type"] == "url" {
                if let Some(url) = value["url"].as_str() {
                    address("link", url, None, self.errors, self.warnings, path);
                }
            } else if value.get("type").is_none()
                && let Some(object) = value.as_object()
            {
                for url in std::iter::once(value.get("url"))
                    .chain(
                        object
                            .values()
                            .filter(|child| child.is_object())
                            .map(|child| child.get("url")),
                    )
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    address("button", url, None, self.errors, self.warnings, path);
                }
            }
        }
        let Some(definition) = name.and_then(|name| self.spec.types().get(name)) else {
            self.text(&value["text"], level, sure, path);
            if let Some(items) = value["blocks"].as_array() {
                self.blocks(items, level, sure, &format!("{path}.blocks"));
            }
            return;
        };
        let fields = definition["fields"].as_array().cloned().unwrap_or_default();
        for field in fields {
            let name = field["name"].as_str().unwrap_or("");
            if let Some(child) = value.get(name) {
                self.field(
                    child,
                    name,
                    field["types"].as_array().map_or(&[], Vec::as_slice),
                    level,
                    sure,
                    &format!("{path}.{name}"),
                );
            }
        }
    }
}

pub(super) fn blocks(
    spec: &Spec,
    items: &[Value],
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut count = Count {
        spec,
        blocks: 0,
        chars: 0,
        longest: 0,
        depth: 0,
        about: 0,
        media: 0,
        errors,
        warnings,
    };
    count.blocks(items, 0, 0, "rich_message.blocks");
    if count.blocks > 500 {
        count.errors.push(format!(
            "блоков вместе с пунктами списков и строками таблиц: {}, лимит 500",
            count.blocks
        ));
    }
    if count.depth > 16 {
        count.errors.push(format!(
            "{} уровней вложенности блоков и форматирования, лимит 16",
            count.depth
        ));
    } else if count.about > 16 {
        count.warnings.push(format!(
            "около {} уровней вложенности, лимит 16; Telegram точно определит глубину",
            count.about
        ));
    }
    Map::from_iter([
        ("blocks".to_owned(), json!(count.blocks)),
        ("chars".to_owned(), json!(count.chars)),
        ("depth".to_owned(), json!(count.depth)),
        ("max_string_bytes".to_owned(), json!(count.longest)),
        ("media".to_owned(), json!(count.media)),
    ])
}

pub(super) fn formula(expression: &str, warnings: &mut Vec<String>, path: &str) {
    let mut depth = 0usize;
    let mut most = 0usize;
    let mut chars = expression.chars().peekable();
    while let Some(ch) = chars.next() {
        let token = if ch == '\\' {
            let mut command = String::from("\\");
            if chars.peek().is_some_and(char::is_ascii_alphabetic) {
                while chars.peek().is_some_and(char::is_ascii_alphabetic) {
                    if let Some(ch) = chars.next() {
                        command.push(ch);
                    }
                }
            } else {
                chars.next();
            }
            command
        } else {
            ch.to_string()
        };
        match token.as_str() {
            "{" | r"\left" | r"\begin" | r"\bgroup" | r"\begingroup" => {
                depth += 1;
                most = most.max(depth);
            }
            "}" | r"\right" | r"\end" | r"\egroup" | r"\endgroup" => {
                depth = depth.saturating_sub(1)
            }
            _ => {}
        }
    }
    if most > 64 {
        warnings.push(format!("{path}: формула глубже 64 уровней вложенности: предпросмотр её не рисует, поведение Telegram не замерено"));
    }
}
