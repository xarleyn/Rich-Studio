use std::collections::BTreeMap;

use regex::Regex;
use serde_json::Value;

use crate::errors::StoreError;

use super::{TemplateError, invalid};

pub(super) fn visit<F>(
    value: &Value,
    path: &mut Vec<String>,
    depth: usize,
    variable: &Regex,
    action: &mut F,
) -> Result<(), TemplateError>
where
    F: FnMut(&str, &[String]) -> Result<(), TemplateError>,
{
    if depth > 60 {
        return invalid("Слишком большая вложенность шаблона.");
    }
    match value {
        Value::String(text) => action(text, path),
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                path.push(index.to_string());
                visit(child, path, depth + 1, variable, action)?;
                path.pop();
            }
            Ok(())
        }
        Value::Object(items) => {
            for (key, child) in items {
                if variable.is_match(key) {
                    return invalid(
                        "Переменные допускаются в значениях, но не в именах полей JSON.",
                    );
                }
                path.push(key.clone());
                visit(child, path, depth + 1, variable, action)?;
                path.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum Mode {
    Plain,
    Html,
    Markdown,
}

impl Mode {
    pub(super) fn for_path(path: &[String]) -> Self {
        match path.last().map(String::as_str) {
            Some("html") => Self::Html,
            Some("markdown") => Self::Markdown,
            _ => Self::Plain,
        }
    }
}

pub(super) fn escape(text: &str, mode: Mode) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        match (mode, ch) {
            (Mode::Html, '&') => output.push_str("&amp;"),
            (Mode::Html, '<') => output.push_str("&lt;"),
            (Mode::Html, '>') => output.push_str("&gt;"),
            (Mode::Html, '"') => output.push_str("&quot;"),
            (Mode::Html, '\'') => output.push_str("&#x27;"),
            (Mode::Markdown, ch) if "&<>\"'\\`*_{}[]()#+.!|~=$-".contains(ch) => {
                output.push_str(&format!("&#{};", u32::from(ch)))
            }
            _ => output.push(ch),
        }
    }
    output
}

type Replacements = BTreeMap<(Mode, String), (String, usize)>;

pub(super) fn substitute(
    value: &mut Value,
    path: &mut Vec<String>,
    variable: &Regex,
    replacements: &Replacements,
) -> Result<(), TemplateError> {
    match value {
        Value::String(text) => {
            let mut replaced = String::new();
            let mut position = 0;
            for capture in variable.captures_iter(text) {
                let matched = capture
                    .get(0)
                    .ok_or_else(|| StoreError::Migration("missing variable match".to_owned()))?;
                replaced.push_str(&text[position..matched.start()]);
                let item = replacements
                    .get(&(Mode::for_path(path), capture[1].to_owned()))
                    .ok_or_else(|| {
                        TemplateError::Invalid(format!("Неизвестное поле: {}", &capture[1]))
                    })?;
                replaced.push_str(&item.0);
                position = matched.end();
            }
            replaced.push_str(&text[position..]);
            *text = replaced;
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                path.push(index.to_string());
                substitute(child, path, variable, replacements)?;
                path.pop();
            }
        }
        Value::Object(items) => {
            for (key, child) in items {
                path.push(key.clone());
                substitute(child, path, variable, replacements)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}
