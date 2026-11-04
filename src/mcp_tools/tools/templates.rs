//! Reusable templates share the editor's literal substitution service.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::AppState;
use crate::services::drafts::draft_links;
use crate::services::templates::{self, FillInput, TemplateInput};

use super::definition;

pub fn definitions() -> Vec<Value> {
    let field = json!({
        "type":"object",
        "properties": {
            "key":{"type":"string","pattern":"^[A-Za-z][A-Za-z0-9_]{0,31}$","description":"Имя переменной {{key}} в строках поста"},
            "label":{"type":"string","minLength":1,"maxLength":100,"description":"Подпись поля в форме"},
            "type":{"type":"string","enum":["text","multiline","url"],"default":"text"},
            "required":{"type":"boolean","default":true},
            "default":{"type":"string","default":"","maxLength":10000}
        },
        "required":["key","label"]
    });
    let mut list = definition(
        "list_post_templates",
        "Шаблоны текущей учётки: id, название, version и заполняемые поля.",
        true,
        false,
        false,
        json!({}),
    );
    list["inputSchema"]["required"] = json!([]);
    let mut get = definition(
        "get_post_template",
        "Шаблон целиком: payload с {{key}}, fields и version перед правкой или заполнением.",
        true,
        false,
        false,
        json!({"template_id":{"type":"string"}}),
    );
    get["inputSchema"]["required"] = json!(["template_id"]);
    let mut save = definition(
        "save_post_template",
        "Создать шаблон поста с полями key, label, type (text/multiline/url), required, default. В посте ставьте {{key}} в строковые значения. Для обновления нужны template_id и прочитанная version. Не публикует и не сохраняет _studio с альтернативными буферами редактора.",
        false,
        false,
        false,
        json!({
            "title":{"type":"string"},"post":{"type":"object"},"fields":{"type":"array","items":field},
            "template_id":{"anyOf":[{"type":"string"},{"type":"null"}],"default":null},
            "version":{"anyOf":[{"type":"integer"},{"type":"null"}],"default":null}
        }),
    );
    save["inputSchema"]["required"] = json!(["title", "post", "fields"]);
    let mut fill = definition(
        "draft_from_template",
        "Заполнить шаблон и создать отдельный черновик без публикации; пропуски берутся из default. Обязательные поля и URL проверяются, HTML/Markdown экранируются. Возвращает ссылку для открытия.",
        false,
        false,
        false,
        json!({
            "template_id":{"type":"string"},"version":{"type":"integer"},
            "values":{"type":"object","additionalProperties":{"type":"string"},"description":"Заполняемые значения по key из fields"},
            "title":{"type":"string","default":""}
        }),
    );
    fill["inputSchema"]["required"] = json!(["template_id", "version", "values"]);
    vec![list, get, save, fill]
}

fn decode<T: DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments).map_err(|error| error.to_string())
}

fn id(arguments: &Value) -> Result<String, String> {
    arguments["template_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "передайте template_id".to_owned())
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    match name {
        "list_post_templates" => {
            templates::run(state.account, |service| Ok(json!(service.list()?)))
                .await
                .map_err(|error| error.to_string())
        }
        "get_post_template" => {
            let id = id(&arguments)?;
            templates::run(state.account, move |service| service.get(&id))
                .await
                .map_err(|error| error.to_string())
        }
        "save_post_template" => {
            if !arguments["fields"].is_array() {
                return Err("передайте fields — список полей шаблона".to_owned());
            }
            let id = if arguments["template_id"].is_null() {
                None
            } else {
                Some(id(&arguments)?)
            };
            let input: TemplateInput = decode(arguments)?;
            templates::run(state.account, move |service| {
                service.save(id.as_deref(), input)
            })
            .await
            .map_err(|error| error.to_string())
        }
        "draft_from_template" => {
            let id = id(&arguments)?;
            if !arguments["values"].is_object() {
                return Err("передайте values — значения полей шаблона".to_owned());
            }
            let input: FillInput = decode(arguments)?;
            let mut draft = templates::run(state.account, move |service| {
                service.create_draft(&id, input)
            })
            .await
            .map_err(|error| error.to_string())?;
            let draft_id = draft["id"].as_str().ok_or("created draft has no id")?;
            let links = draft_links(&state.settings.public_url, draft_id);
            if let (Some(draft), Some(links)) = (draft.as_object_mut(), links.as_object()) {
                draft.extend(links.clone());
            }
            Ok(draft)
        }
        _ => Err(format!("unknown template tool {name}")),
    }
}
