//! A private forward must consume an account-local, explicitly armed ticket.

use super::{import_links::parse_link, imports};
use crate::{
    account_context::{AccountContext, Role},
    errors::ImportError,
    settings::Settings,
    spec::source::Spec,
    store::{auth, import_inbox},
};
use serde_json::{Value, json};

pub fn begin(
    settings: &Settings,
    context: &AccountContext,
    url: &str,
) -> Result<Value, ImportError> {
    let link = parse_link(url)?;
    if imports::status(settings, context)["forward_available"] != true {
        return Err(ImportError::new(
            409,
            "Импорт пересылкой сейчас недоступен.",
        ));
    }
    let sender = context
        .account_id
        .or_else(|| settings.preview_chat.parse::<i64>().ok())
        .ok_or_else(|| ImportError::new(409, "Не настроен получатель пересылки."))?;
    let ticket = import_inbox::create(context, &link.url(), sender, auth::now())?;
    Ok(
        json!({"id":ticket["id"],"expires_at":ticket["expires_at"],"bot_url":format!("https://t.me/{}?start=import_{}",settings.bot_username,ticket["id"].as_str().unwrap_or(""))}),
    )
}
pub fn get(context: &AccountContext, token: &str) -> Result<Value, ImportError> {
    let ticket = import_inbox::get(context, token, auth::now())?
        .ok_or_else(|| ImportError::new(404, "Импорт не найден."))?;
    Ok(
        json!({"id":ticket["id"],"state":ticket["state"],"result":ticket["result"],"error":ticket["error"],"expires_at":ticket["expires_at"]}),
    )
}

fn receive_sync(settings: &Settings, update: &Value, spec: &Spec) -> Result<bool, ImportError> {
    let message = &update["message"];
    let Some(sender) = message["from"]["id"].as_i64() else {
        return Ok(false);
    };
    if message["chat"]["type"] != "private"
        || message["chat"]["id"].as_i64() != Some(sender)
        || message["from"]["is_bot"] == true
    {
        return Ok(false);
    }
    let context = if settings.public && auth::account(settings, sender)?.is_some() {
        AccountContext::account(settings, sender, Role::Owner)
    } else if settings.preview_chat == sender.to_string() {
        AccountContext::root(settings)
    } else {
        return Ok(false);
    };
    if let Some(token) = message["text"]
        .as_str()
        .and_then(|text| text.strip_prefix("/start import_"))
    {
        return import_inbox::arm(&context, token.trim(), sender, auth::now());
    }
    if message["forward_origin"].is_null() {
        return Ok(false);
    }
    let Some(ticket) = import_inbox::claim(&context, sender, auth::now())? else {
        return Ok(false);
    };
    let result = (|| {
        let link = parse_link(ticket["source_url"].as_str().unwrap_or(""))?;
        let origin = &message["forward_origin"];
        let mut warnings = Vec::new();
        if origin["type"] == "channel" {
            let matches = if let Some(username) = &link.username {
                origin["chat"]["username"]
                    .as_str()
                    .is_some_and(|peer| peer.eq_ignore_ascii_case(username))
            } else {
                origin["chat"]["id"].as_i64().map(|peer| peer.to_string())
                    == link.channel_id.map(|id| format!("-100{id}"))
            };
            if !matches || origin["message_id"].as_i64() != Some(i64::from(link.message_id)) {
                return Err(ImportError::new(
                    400,
                    "Переслано другое сообщение. Начните импорт заново.",
                ));
            }
        } else {
            warnings.push(
                "Telegram скрыл источник пересылки; соответствие ссылке не подтверждено.".into(),
            );
        }
        imports::from_forward(&context, spec, message, &link, warnings)
    })();
    let token = ticket["id"].as_str().unwrap_or("");
    match result {
        Ok(result) => import_inbox::finish(&context, token, Some(&result), None)?,
        Err(error) => import_inbox::finish(&context, token, None, Some(&error.to_string()))?,
    };
    Ok(true)
}
pub async fn receive(
    settings: &Settings,
    update: &Value,
    spec: &Spec,
) -> Result<bool, ImportError> {
    let settings = settings.clone();
    let update = update.clone();
    let spec = spec.clone();
    tokio::task::spawn_blocking(move || receive_sync(&settings, &update, &spec))
        .await
        .map_err(|_| ImportError::new(500, "Обработка пересылки прервана."))?
}
