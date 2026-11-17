use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{TemplateError, invalid};

pub const MAX_VALUE: usize = 10_000;
pub(super) const FILE_VARIABLE: &str = "Для файла замените всю ссылку upload:имя одним полем {{file}}; задайте полную ссылку в значении по умолчанию.";

fn default_true() -> bool {
    true
}
fn default_text() -> String {
    "text".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct TemplateField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type", default = "default_text")]
    pub kind: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub default: String,
}

#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct TemplateInput {
    pub title: String,
    pub post: Value,
    #[serde(default)]
    pub fields: Vec<TemplateField>,
    pub version: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, utoipa::ToSchema)]
pub struct FillInput {
    pub version: i64,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub title: String,
}

pub(super) fn field_value(
    field: &TemplateField,
    value: &str,
    required: bool,
) -> Result<(), TemplateError> {
    if value.chars().count() > MAX_VALUE {
        return invalid(format!(
            "Поле «{}»: допустимо не больше {MAX_VALUE} символов.",
            field.label
        ));
    }
    if required && field.required && value.trim().is_empty() {
        return invalid(format!("Заполните поле «{}».", field.label));
    }
    if !value.is_empty() && field.kind == "url" {
        let valid = value
            .strip_prefix("upload:")
            .is_some_and(crate::telegram::uploads::valid_name)
            || value.starts_with('#')
            || value.starts_with('/') && !value.starts_with("//")
            || url::Url::parse(value).is_ok_and(|parsed| match parsed.scheme() {
                "http" | "https" => {
                    value
                        .split_once("://")
                        .is_some_and(|(_, rest)| !rest.starts_with('/'))
                        && parsed.host_str().is_some_and(|host| !host.is_empty())
                }
                "tg" | "mailto" | "tel" => true,
                _ => false,
            });
        if !valid
            || value
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control() || "<>\"'\\".contains(ch))
        {
            return invalid(format!(
                "Поле «{}»: укажите ссылку http(s)://, tg://, mailto:, tel:, #якорь, /путь или upload:имя без пробелов.",
                field.label
            ));
        }
    }
    Ok(())
}
