use serde_json::{Value, json};

use crate::AppState;
use crate::spec::source::Spec;
use crate::spec::validator::Validator;

use super::definition;

pub fn definitions() -> Vec<Value> {
    vec![
        definition(
            "bot_api_status",
            "Версия Bot API, на которой работает студия, и последние изменения rich-типов и методов.",
            true,
            false,
            false,
            json!({}),
        ),
        definition(
            "refresh_bot_api",
            "Скачать свежую спецификацию Bot API; при ошибке сохранить текущую версию.",
            false,
            false,
            true,
            json!({}),
        ),
        definition(
            "describe_type",
            "Точные поля типа или метода Bot API из текущей спецификации.",
            true,
            false,
            false,
            json!({"name": {"type": "string"}}),
        ),
    ]
}

pub fn status(spec: &Spec) -> Value {
    let summary = spec.summary();
    json!({
        "version": spec.version(),
        "release_date": spec.data.get("release_date").cloned().unwrap_or(Value::Null),
        "source": spec.source,
        "changes": spec.changes.iter().take(3).cloned().collect::<Vec<_>>(),
        "gaps": summary["gaps"],
        "last_check": spec.last_check,
    })
}

pub fn describe(spec: &Spec, name: &str) -> Result<Value, String> {
    spec.definition(name)
        .ok_or_else(|| format!("{name} нет в спецификации {}", spec.version()))
}

pub fn validate(spec: &Spec, post: &Value) -> Value {
    Validator::new(spec).check_post(post)
}

pub async fn refresh(state: &AppState) -> Value {
    crate::services::specs::refresh(state, true).await
}
