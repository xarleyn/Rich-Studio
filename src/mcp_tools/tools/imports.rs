use super::definition;
use crate::{
    AppState,
    services::{drafts::draft_links, imports},
};
use serde_json::{Value, json};

pub fn definitions() -> Vec<Value> {
    let mut status = definition(
        "telegram_import_status",
        "Подключён ли Telegram для импорта в текущей учётке; доступна ли пересылка боту.",
        true,
        false,
        false,
        json!({}),
    );
    status["inputSchema"]["required"] = json!([]);
    let mut import = definition(
        "import_telegram_post",
        "Импортировать Telegram-пост по ссылке в новый редактируемый черновик. Сохраняет блоки, форматирование и медиа, возвращает предупреждения о неполном содержимом. Не публикует.",
        false,
        false,
        true,
        json!({"url":{"type":"string","maxLength":2048}}),
    );
    import["inputSchema"]["required"] = json!(["url"]);
    vec![status, import]
}
pub async fn call(state: AppState, name: &str, args: Value) -> Result<Value, String> {
    match name {
        "telegram_import_status" => {
            let settings = state.settings.clone();
            let context = state.account.clone();
            tokio::task::spawn_blocking(move || imports::status(&settings, &context))
                .await
                .map_err(|_| "Проверка импорта прервана.".into())
        }
        "import_telegram_post" => match args["url"].as_str() {
            Some(url) => imports::import_post(&state.settings, &state.account, url)
                .await
                .map(|mut result| {
                    let id = result["draft"]["id"].as_str().unwrap_or("");
                    let links = draft_links(&state.settings.public_url, id);
                    if let Some(object) = result.as_object_mut() {
                        object.extend(
                            links
                                .as_object()
                                .into_iter()
                                .flat_map(|object| object.clone()),
                        );
                    }
                    result
                })
                .map_err(|error| error.to_string()),
            None => Err("url: нужна ссылка Telegram".into()),
        },
        _ => Err("неизвестный инструмент импорта".into()),
    }
}
