use std::fmt;

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;
use uuid::Uuid;

use crate::account_context::Role;
use crate::errors::StoreError;
use crate::settings::Settings;
use crate::store::{account_tokens, auth};

#[derive(Clone, Debug)]
pub struct Principal {
    pub name: Option<String>,
    pub role: Role,
    pub telegram_id: Option<i64>,
    pub account_session: bool,
}

impl Principal {
    pub fn owner() -> Self {
        Self {
            name: None,
            role: Role::Admin,
            telegram_id: None,
            account_session: false,
        }
    }

    pub fn can(&self, role: Role) -> bool {
        self.role.rank() >= role.rank()
    }

    pub fn level(&self) -> &'static str {
        self.role.level()
    }

    pub fn actor(&self, client: &str) -> String {
        match &self.name {
            Some(name) if client.is_empty() => name.clone(),
            Some(name) => format!("{client}@{name}"),
            None if client.is_empty() => "api".to_owned(),
            None => client.to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct AuthError {
    pub status: u16,
    pub message: String,
}

impl AuthError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuthError {}

impl From<StoreError> for AuthError {
    fn from(error: StoreError) -> Self {
        Self::new(
            if matches!(error, StoreError::StorageFull) {
                413
            } else {
                500
            },
            error.to_string(),
        )
    }
}

pub fn equal_secret(left: &str, right: &str) -> bool {
    let left = Sha256::digest(left.as_bytes());
    let right = Sha256::digest(right.as_bytes());
    bool::from(left.as_slice().ct_eq(right.as_slice()))
}

pub fn secret(prefix: &str) -> String {
    let mut bytes = [0; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

pub fn auth_on(settings: &Settings) -> Result<bool, AuthError> {
    Ok(settings.public || !settings.api_key.is_empty() || auth::any_tokens(settings)?)
}

fn token_principal(token: &Value, account: bool) -> Option<Principal> {
    Some(Principal {
        name: Some(token["name"].as_str()?.to_owned()),
        role: Role::parse(token["level"].as_str()?)?,
        telegram_id: if account {
            Some(token["telegram_id"].as_i64()?)
        } else {
            None
        },
        account_session: false,
    })
}

pub fn by_secret(settings: &Settings, secret: &str) -> Result<Option<Principal>, AuthError> {
    if !settings.api_key.is_empty() && equal_secret(secret, &settings.api_key) {
        return Ok(Some(Principal::owner()));
    }
    if let Some(token) = auth::token_by_secret(settings, secret)? {
        return Ok(token_principal(&token, false));
    }
    if settings.public
        && let Some(token) = account_tokens::by_secret(settings, secret)?
    {
        return Ok(token_principal(&token, true));
    }
    Ok(None)
}

pub fn by_session(settings: &Settings, secret: &str) -> Result<Option<Principal>, AuthError> {
    if settings.public
        && let Some(user) = auth::account_session(settings, secret)?
    {
        let uid = user["telegram_id"]
            .as_i64()
            .ok_or_else(|| AuthError::new(500, "invalid account identity"))?;
        return Ok(Some(Principal {
            name: Some(format!("tg:{uid}")),
            role: Role::Publish,
            telegram_id: Some(uid),
            account_session: true,
        }));
    }
    let Some(session) = auth::session_by_secret(settings, secret)? else {
        return Ok(None);
    };
    if let Some(uid) = session["telegram_id"].as_i64() {
        return Ok((uid == settings.owner_telegram_id && uid > 0).then(Principal::owner));
    }
    if let Some(name) = session["token_name"].as_str() {
        return Ok(session["level"]
            .as_str()
            .and_then(Role::parse)
            .map(|role| Principal {
                name: Some(name.to_owned()),
                role,
                telegram_id: None,
                account_session: false,
            }));
    }
    let valid_key = !settings.api_key.is_empty()
        && session["key_hash"]
            .as_str()
            .is_some_and(|hash| equal_secret(hash, &auth::hash_secret(&settings.api_key)));
    Ok(valid_key.then(Principal::owner))
}

pub fn require(who: &Principal, role: Role) -> Result<(), AuthError> {
    if who.can(role) {
        Ok(())
    } else {
        Err(AuthError::new(
            403,
            format!(
                "у токена {} нет права {}",
                who.name.as_deref().unwrap_or(""),
                role.level()
            ),
        ))
    }
}

pub fn browser_account(settings: &Settings, who: &Principal) -> Result<i64, AuthError> {
    who.telegram_id
        .filter(|_| settings.public && who.account_session)
        .ok_or_else(|| {
            AuthError::new(
                403,
                "Управление токенами доступно после входа через Telegram",
            )
        })
}

pub fn login(settings: &Settings, key: &str) -> Result<(String, Value), AuthError> {
    if key.is_empty() || key.chars().count() > 200 {
        return Err(AuthError::new(422, "key: от 1 до 200 символов"));
    }
    let who = by_secret(settings, key)?
        .ok_or_else(|| AuthError::new(401, "ключ или токен не подошёл"))?;
    if who.telegram_id.is_some() {
        return Err(AuthError::new(
            403,
            "Персональный токен предназначен для API/MCP. В редактор войдите через Telegram.",
        ));
    }
    let session = secret("");
    let hash = who
        .name
        .is_none()
        .then(|| auth::hash_secret(&settings.api_key));
    auth::add_session(
        settings,
        &session,
        who.name.as_deref(),
        (settings.session_days * 86400.0) as i64,
        hash.as_deref(),
        None,
    )?;
    Ok((
        session,
        serde_json::json!({"name":who.name,"level":who.level()}),
    ))
}

pub fn whoami(settings: &Settings, who: &Principal) -> Result<Value, AuthError> {
    let mut result =
        serde_json::json!({"name":who.name,"level":who.level(),"auth":auth_on(settings)?});
    if let Some(uid) = who.telegram_id {
        result["user"] = auth::account(settings, uid)?.unwrap_or(Value::Null);
    }
    Ok(result)
}

pub fn logout(settings: &Settings, secret: Option<&str>) -> Result<Value, AuthError> {
    if let Some(secret) = secret {
        auth::logout(settings, secret)?;
    }
    Ok(serde_json::json!({"ok":true}))
}
