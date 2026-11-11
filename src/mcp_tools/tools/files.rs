use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::AppState;
use crate::services::media::MediaService;

use super::definition;

pub fn definitions() -> Vec<Value> {
    vec![
        definition(
            "upload_media",
            "Загрузить картинку, видео или файл для поста; вернуть ref upload:<имя>.",
            false,
            false,
            true,
            json!({"url":{"type":"string"},"data_base64":{"type":"string"},"filename":{"type":"string"}}),
        ),
        definition(
            "list_media",
            "Загруженные файлы; used — нужен ли файл черновику или посту.",
            true,
            false,
            false,
            json!({}),
        ),
    ]
}

fn service(state: &AppState) -> MediaService {
    MediaService::new(
        state.account.clone(),
        state.settings.max_upload_mb,
        state.settings.account_storage_mb,
    )
}

pub async fn call(state: AppState, name: &str, arguments: Value) -> Result<Value, String> {
    let service = service(&state);
    match name {
        "upload_media" => {
            let url = arguments.get("url").and_then(Value::as_str);
            let encoded = arguments.get("data_base64").and_then(Value::as_str);
            if url.is_some() == encoded.is_some() {
                return Err("передайте url или data_base64, одно из двух".to_owned());
            }
            let filename = arguments.get("filename").and_then(Value::as_str);
            if let Some(encoded) = encoded {
                let filename = filename
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        "с data_base64 нужен filename с расширением, например cover.jpg".to_owned()
                    })?;
                let encoded = encoded.to_owned();
                tokio::task::spawn_blocking(move || {
                    let data = STANDARD
                        .decode(encoded)
                        .map_err(|_| "data_base64: не base64".to_owned())?;
                    service
                        .upload(&filename, &data)
                        .map_err(|error| error.to_string())
                })
                .await
                .map_err(|error| format!("upload worker failed: {error}"))?
            } else {
                service
                    .fetch(
                        url.unwrap_or_default(),
                        filename,
                        false,
                        state.settings.media_fetch_private,
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
        }
        "list_media" => tokio::task::spawn_blocking(move || service.list())
            .await
            .map_err(|error| format!("media worker failed: {error}"))?
            .map(|files| json!(files))
            .map_err(|error| error.to_string()),
        _ => Err(format!("unknown media tool {name}")),
    }
}
