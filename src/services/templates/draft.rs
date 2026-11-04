use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::errors::StoreError;
use crate::store::{drafts, templates as records};

use super::fields::field_value;
use super::rules::Rules;
use super::substitute::{Mode, escape, substitute, visit};
use super::{FillInput, TemplateError, TemplateField, TemplateService, invalid, version_valid};

impl TemplateService {
    pub fn create_draft(&self, id: &str, input: FillInput) -> Result<Value, TemplateError> {
        version_valid(input.version)?;
        let template = self.get(id)?;
        if template["version"].as_i64() != Some(input.version) {
            return Err(TemplateError::Conflict(
                "Шаблон изменился. Откройте форму заново, чтобы заполнить актуальные поля."
                    .to_owned(),
            ));
        }
        let fields: Vec<TemplateField> = serde_json::from_value(template["fields"].clone())
            .map_err(|error| {
                StoreError::Migration(format!("invalid stored template fields: {error}"))
            })?;
        let known = fields
            .iter()
            .map(|field| field.key.as_str())
            .collect::<BTreeSet<_>>();
        let extra = input
            .values
            .keys()
            .filter(|key| !known.contains(key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !extra.is_empty() {
            return invalid(format!("Неизвестные поля: {}", extra.join(", ")));
        }
        let mut replacements = BTreeMap::new();
        for field in fields {
            let value = input.values.get(&field.key).unwrap_or(&field.default);
            field_value(&field, value, true)?;
            for mode in [Mode::Plain, Mode::Html, Mode::Markdown] {
                let text = escape(value, mode);
                let size = records::encode(&Value::String(text.clone()))?.len() - 2;
                replacements.insert((mode, field.key.clone()), (text, size));
            }
        }
        let rules = Rules::new()?;
        let payload = &template["payload"];
        let mut size = records::encode(payload)?.len() as i64;
        visit(
            payload,
            &mut Vec::new(),
            0,
            &rules.variable,
            &mut |text, path| {
                let mode = Mode::for_path(path);
                for capture in rules.variable.captures_iter(text) {
                    let replacement = replacements
                        .get(&(mode, capture[1].to_owned()))
                        .ok_or_else(|| {
                            TemplateError::Invalid(format!(
                                "Опишите поля для переменных: {}",
                                &capture[1]
                            ))
                        })?;
                    size += replacement.1 as i64 - capture[0].len() as i64;
                }
                Ok(())
            },
        )?;
        if size > records::MAX_BYTES as i64 {
            return Err(TemplateError::TooLarge(
                "Заполненный пост больше 1 МБ.".to_owned(),
            ));
        }
        let mut post = payload.clone();
        substitute(&mut post, &mut Vec::new(), &rules.variable, &replacements)?;
        let title = if input.title.trim().is_empty() {
            template["title"]
                .as_str()
                .ok_or_else(|| StoreError::Migration("invalid stored template title".to_owned()))?
        } else {
            input.title.trim()
        };
        if title.chars().count() > 128 {
            return invalid("Название черновика длиннее 128 символов.");
        }
        drafts::save(&self.context, None, title, &post, None, None, None)?
            .ok_or_else(|| StoreError::Migration("created draft is missing".to_owned()).into())
    }
}
