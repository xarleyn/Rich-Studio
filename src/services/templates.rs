//! Literal template substitution. Templates create independent unpublished drafts.
//!
//! Фасад: подмодули приватные, публичные пути `crate::services::templates::*` сохраняются.

mod draft;
mod fields;
mod rules;
mod substitute;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::account_context::AccountContext;
use crate::errors::StoreError;
use crate::store::templates as records;
pub use crate::store::templates::TemplateError;

use fields::{FILE_VARIABLE, field_value};
use rules::Rules;
use substitute::visit;

pub use fields::{FillInput, MAX_VALUE, TemplateField, TemplateInput};

pub struct TemplateService {
    context: AccountContext,
}

/// Every HTTP/MCP operation owns and joins its blocking SQLite worker.
pub async fn run<T, F>(context: AccountContext, action: F) -> Result<T, TemplateError>
where
    T: Send + 'static,
    F: FnOnce(&TemplateService) -> Result<T, TemplateError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || action(&TemplateService::new(context)))
        .await
        .map_err(|error| StoreError::Migration(format!("template worker failed: {error}")))?
}

impl TemplateService {
    pub fn new(context: AccountContext) -> Self {
        Self { context }
    }

    pub fn list(&self) -> Result<Vec<Value>, TemplateError> {
        records::list(&self.context)
    }

    pub fn get(&self, id: &str) -> Result<Value, TemplateError> {
        records::get(&self.context, id)
    }

    pub fn delete(&self, id: &str, version: i64) -> Result<(), TemplateError> {
        version_valid(version)?;
        records::delete(&self.context, id, version)
    }

    pub fn save(&self, id: Option<&str>, mut input: TemplateInput) -> Result<Value, TemplateError> {
        let rules = Rules::new()?;
        input.title = input.title.trim().to_owned();
        if input.title.is_empty() || input.title.chars().count() > 128 {
            return invalid("Название шаблона должно содержать от 1 до 128 символов.");
        }
        if input.fields.len() > 50 {
            return invalid("В шаблоне допускается не больше 50 полей.");
        }
        if let Some(version) = input.version {
            version_valid(version)?;
        }
        let post = input
            .post
            .as_object_mut()
            .ok_or_else(|| TemplateError::Invalid("Пост должен быть объектом JSON.".to_owned()))?;
        post.remove("_studio");
        let formats = post
            .get("rich_message")
            .and_then(Value::as_object)
            .map(|rich| {
                ["blocks", "html", "markdown"]
                    .iter()
                    .filter(|key| rich.contains_key(**key))
                    .count()
            });
        if formats != Some(1) {
            return invalid(
                "Шаблон должен содержать rich_message и один формат: blocks, html или markdown.",
            );
        }
        let mut used = BTreeSet::new();
        visit(
            &input.post,
            &mut Vec::new(),
            0,
            &rules.variable,
            &mut |text, _path| {
                if text.starts_with("upload:") && rules.variable.is_match(text) {
                    return invalid(FILE_VARIABLE);
                }
                used.extend(
                    rules
                        .variable
                        .captures_iter(text)
                        .map(|capture| capture[1].to_owned()),
                );
                Ok(())
            },
        )?;
        let mut keys = BTreeSet::new();
        for field in &mut input.fields {
            field.label = field.label.trim().to_owned();
            if !rules.key.is_match(&field.key) || !keys.insert(field.key.clone()) {
                return invalid(
                    "Имена полей должны быть уникальными: латинская буква, затем буквы, цифры или _; до 32 знаков.",
                );
            }
            if field.label.is_empty()
                || field.label.chars().count() > 100
                || !["text", "multiline", "url"].contains(&field.kind.as_str())
            {
                return invalid(format!("Проверьте подпись и тип поля {}.", field.key));
            }
            field_value(field, &field.default, false)?;
        }
        if used != keys {
            let missing = used.difference(&keys).cloned().collect::<Vec<_>>();
            if !missing.is_empty() {
                return invalid(format!(
                    "Опишите поля для переменных: {}",
                    missing.join(", ")
                ));
            }
            let unused = keys.difference(&used).cloned().collect::<Vec<_>>();
            return invalid(format!(
                "Эти поля не используются в посте: {}",
                unused.join(", ")
            ));
        }
        let by_key = input
            .fields
            .iter()
            .map(|field| (field.key.as_str(), field))
            .collect::<BTreeMap<_, _>>();
        visit(
            &input.post,
            &mut Vec::new(),
            0,
            &rules.variable,
            &mut |text, path| rules.validate_source(text, path, &by_key),
        )?;
        if id.is_some() && input.version.is_none() {
            return invalid("Для обновления передайте version прочитанного шаблона.");
        }
        let fields = serde_json::to_value(&input.fields)
            .map_err(|error| StoreError::Migration(error.to_string()))?;
        records::save(
            &self.context,
            &input.title,
            &input.post,
            &fields,
            id,
            input.version,
        )
    }
}

pub fn version_valid(version: i64) -> Result<(), TemplateError> {
    if version < 1 {
        invalid("Версия шаблона должна быть не меньше 1.")
    } else {
        Ok(())
    }
}

fn invalid<T>(message: impl Into<String>) -> Result<T, TemplateError> {
    Err(TemplateError::Invalid(message.into()))
}
