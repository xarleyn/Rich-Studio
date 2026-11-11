use super::draft::save_result;
use crate::{
    account_context::AccountContext,
    errors::ImportError,
    services::import_links::MessageLink,
    spec::{import_content::entities_text, source::Spec},
};
use serde_json::{Value, json};

fn without_actions(value: &mut Value, warnings: &mut Vec<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                without_actions(item, warnings)
            }
        }
        Value::Object(object) => {
            if ["buttons", "button"]
                .contains(&object.get("type").and_then(Value::as_str).unwrap_or(""))
            {
                warnings.push(
                    "Кнопки исходного бота заменены отметками; их действия не перенесены.".into(),
                );
                *value = if object.get("type") == Some(&json!("buttons")) {
                    json!({"type":"paragraph","text":"[Кнопки исходного бота]"})
                } else {
                    json!("[Кнопка исходного бота]")
                };
            } else {
                for child in object.values_mut() {
                    without_actions(child, warnings)
                }
            }
        }
        _ => {}
    }
}
pub fn from_forward(
    context: &AccountContext,
    spec: &Spec,
    message: &Value,
    link: &MessageLink,
    mut warnings: Vec<String>,
) -> Result<Value, ImportError> {
    if message["has_protected_content"].as_bool() == Some(true) {
        return Err(ImportError::new(
            422,
            "Импорт защищённого содержимого недоступен.",
        ));
    }
    let mut blocks =
        if let Some(rich) = message.get("rich_message").filter(|value| !value.is_null()) {
            let mut blocks = json!(crate::spec::convert::to_input_blocks(
                spec,
                rich["blocks"].as_array().map(Vec::as_slice).unwrap_or(&[])
            ));
            without_actions(&mut blocks, &mut warnings);
            blocks.as_array().cloned().unwrap_or_default()
        } else {
            let mut blocks = Vec::new();
            let text = message["text"]
                .as_str()
                .filter(|text| !text.is_empty())
                .or_else(|| message["caption"].as_str())
                .unwrap_or("");
            if !text.is_empty() {
                let entities = message["entities"]
                    .as_array()
                    .or_else(|| message["caption_entities"].as_array())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                blocks.push(
                    json!({"type":"paragraph","text":entities_text(text,entities,&mut warnings)?}),
                );
            }
            for kind in ["animation", "photo", "video", "audio", "voice", "document"] {
                if let Some(item) = message.get(kind).filter(|value| !value.is_null()) {
                    let item = item
                        .as_array()
                        .and_then(|items| {
                            items.iter().max_by_key(|item| {
                                item["width"]
                                    .as_u64()
                                    .unwrap_or(0)
                                    .saturating_mul(item["height"].as_u64().unwrap_or(0))
                            })
                        })
                        .unwrap_or(item);
                    let target = if kind == "voice" { "voice_note" } else { kind };
                    let mut block = json!({"type":target});
                    block[target] = json!({"type":target,"media":item["file_id"]});
                    blocks.insert(0, block);
                    break;
                }
            }
            blocks
        };
    if !message["media_group_id"].is_null() {
        warnings.push("Пересылка импортирует одно сообщение альбома. Для всего альбома используйте импорт по ссылке.".into());
    }
    if !message["reply_markup"].is_null() {
        warnings.push("Кнопки исходного бота не перенесены.".into());
    }
    warnings.push("Содержимое получено из выбранной вами пересылки.".into());
    save_result(
        context,
        std::mem::take(&mut blocks),
        warnings,
        link,
        "forward",
        "",
    )
}
