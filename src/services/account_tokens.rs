use serde_json::{Value, json};

use crate::services::auth::{self, AuthError, Principal};
use crate::settings::Settings;
use crate::store::{account_tokens as tokens, auth as store_auth};

pub const MAX_ACTIVE: i64 = 20;

pub fn listing(settings: &Settings, who: &Principal) -> Result<Value, AuthError> {
    let uid = auth::browser_account(settings, who)?;
    Ok(
        json!({"tokens":tokens::list_for(settings,uid)?,"mcp_url":format!("{}/mcp",settings.public_url)}),
    )
}

pub fn create(settings: &Settings, who: &Principal, body: &Value) -> Result<Value, AuthError> {
    let uid = auth::browser_account(settings, who)?;
    let object = body
        .as_object()
        .ok_or_else(|| AuthError::new(422, "ожидался объект JSON"))?;
    if object
        .keys()
        .any(|key| !["name", "level", "expires_in_days"].contains(&key.as_str()))
    {
        return Err(AuthError::new(422, "неизвестное поле токена"));
    }
    let name = body["name"].as_str().unwrap_or("").trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(AuthError::new(
            422,
            "Название должно содержать от 1 до 80 видимых символов без управляющих символов",
        ));
    }
    let level = match body.get("level") {
        None => "draft",
        Some(value) => value
            .as_str()
            .ok_or_else(|| AuthError::new(422, "level: read, draft или publish"))?,
    };
    if !["read", "draft", "publish"].contains(&level) {
        return Err(AuthError::new(422, "level: read, draft или publish"));
    }
    let days = match body.get("expires_in_days") {
        None => Some(90),
        Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .filter(|days| (1..=365).contains(days))
                .ok_or_else(|| {
                    AuthError::new(422, "expires_in_days: целое число от 1 до 365 или null")
                })?,
        ),
    };
    if !store_auth::take_budget(settings, &format!("tokens:{uid}"), 60, 3600)? {
        return Err(AuthError::new(
            429,
            "Слишком много новых токенов. Попробуйте через час.",
        ));
    }
    let secret = auth::secret("rsu_");
    let id = uuid::Uuid::new_v4().simple().to_string();
    let expires = days.map(|days| store_auth::now() + days * 86400);
    if !tokens::add(
        settings,
        tokens::NewToken {
            uid,
            id: &id,
            name,
            secret: &secret,
            level,
            expires,
        },
        MAX_ACTIVE,
    )? {
        return Err(AuthError::new(
            409,
            format!("Можно иметь не больше {MAX_ACTIVE} действующих токенов. Отзовите ненужный."),
        ));
    }
    let token = tokens::list_for(settings, uid)?
        .into_iter()
        .find(|token| token["id"] == id)
        .ok_or_else(|| AuthError::new(500, "созданный токен не найден"))?;
    Ok(json!({"token":token,"secret":secret}))
}

pub fn revoke(settings: &Settings, who: &Principal, id: &str) -> Result<Value, AuthError> {
    let uid = auth::browser_account(settings, who)?;
    if !tokens::revoke(settings, uid, id)? {
        return Err(AuthError::new(404, "Токен не найден"));
    }
    Ok(json!({"ok":true}))
}
