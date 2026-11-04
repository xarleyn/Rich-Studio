use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::account_context::{AccountContext, Role};
use crate::services::auth::AuthError;
use crate::settings::Settings;
use crate::store::{auth, db};

pub const LOGIN_TTL: i64 = 300;
const LOGIN_FIELDS: &[&str] = &[
    "id",
    "first_name",
    "last_name",
    "username",
    "photo_url",
    "auth_date",
];

pub fn verify_login(
    settings: &Settings,
    data: &BTreeMap<String, String>,
) -> Result<Value, AuthError> {
    let fields = data
        .iter()
        .filter(|(key, _)| LOGIN_FIELDS.contains(&key.as_str()))
        .collect::<BTreeMap<_, _>>();
    let check = fields
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let key = Sha256::digest(settings.bot_token.as_bytes());
    let mut mac = Hmac::<Sha256>::new_from_slice(&key)
        .map_err(|_| AuthError::new(500, "cannot initialize login verifier"))?;
    mac.update(check.as_bytes());
    let given = data.get("hash").map(String::as_str).unwrap_or("");
    let bytes = if given.len() == 64 && given.is_ascii() {
        (0..32)
            .map(|index| u8::from_str_radix(&given[index * 2..index * 2 + 2], 16))
            .collect::<Result<Vec<_>, _>>()
            .ok()
    } else {
        None
    };
    if settings.bot_token.is_empty()
        || bytes
            .as_deref()
            .is_none_or(|bytes| mac.verify_slice(bytes).is_err())
    {
        return Err(AuthError::new(
            401,
            "Telegram не подтвердил вход. Попробуйте войти ещё раз.",
        ));
    }
    let uid = data
        .get("id")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|uid| *uid > 0 && *uid < (1_i64 << 52));
    let at = data
        .get("auth_date")
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|at| (-30..=LOGIN_TTL).contains(&auth::now().saturating_sub(*at)));
    if uid.is_none() || at.is_none() || data.get("first_name").is_none_or(String::is_empty) {
        return Err(AuthError::new(
            401,
            "Данные входа устарели или повреждены. Войдите ещё раз.",
        ));
    }
    let mut user = serde_json::Map::new();
    for (key, value) in fields {
        user.insert(key.clone(), json!(value));
    }
    user.insert("id".to_owned(), json!(uid));
    Ok(Value::Object(user))
}

pub fn initialize(settings: &Settings, uid: i64) -> Result<(), AuthError> {
    let context = AccountContext::account(settings, uid, Role::Publish);
    let mut connection = db::Store::connect(&context)?;
    db::migrate(&mut connection)?;
    std::fs::create_dir_all(&context.media_dir)
        .map_err(|error| AuthError::new(500, error.to_string()))?;
    Ok(())
}

pub fn all_contexts(settings: &Settings) -> Result<Vec<AccountContext>, AuthError> {
    let mut contexts = vec![AccountContext::root(settings)];
    if settings.public {
        contexts.extend(
            auth::account_ids(settings)?
                .into_iter()
                .map(|uid| AccountContext::account(settings, uid, Role::Publish)),
        );
    }
    Ok(contexts)
}

pub fn start_login(settings: &Settings) -> Result<(String, Value), AuthError> {
    if !settings.public {
        return Err(AuthError::new(404, "Вход через Telegram выключен"));
    }
    let secret = crate::services::auth::secret("");
    auth::challenge(settings, &secret, LOGIN_TTL)?;
    let result = json!({"bot_username":settings.bot_username,"auth_url":format!("{}/auth/telegram/callback?state={secret}",settings.public_url)});
    Ok((secret, result))
}

pub fn finish_login(
    settings: &Settings,
    data: &BTreeMap<String, String>,
    cookie: &str,
) -> Result<String, AuthError> {
    if !settings.public {
        return Err(AuthError::new(404, "Вход через Telegram выключен"));
    }
    let state = data.get("state").map(String::as_str).unwrap_or("");
    if state.is_empty() || cookie.is_empty() || !crate::services::auth::equal_secret(state, cookie)
    {
        return Err(AuthError::new(401, "Начните вход в этом браузере заново."));
    }
    let user = verify_login(settings, data)?;
    if !auth::consume_challenge(settings, state)? {
        return Err(AuthError::new(
            401,
            "Этот вход уже завершён или устарел. Войдите ещё раз.",
        ));
    }
    let uid = user["id"]
        .as_i64()
        .ok_or_else(|| AuthError::new(401, "Данные входа повреждены"))?;
    let secret = crate::services::auth::secret("");
    let ttl = (settings.session_days * 86400.0) as i64;
    if settings.owner_telegram_id > 0 && settings.owner_telegram_id == uid {
        auth::add_session(settings, &secret, None, ttl, None, Some(uid))?;
    } else {
        initialize(settings, uid)?;
        auth::save_account(settings, &user)?;
        auth::add_account_session(settings, &secret, uid, ttl)?;
    }
    Ok(secret)
}
