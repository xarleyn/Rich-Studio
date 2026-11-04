use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::account_context::AccountContext;
use crate::errors::ServiceError;
use crate::services::blocking;
use crate::store::idempotency;

pub const MAX_KEY: usize = 200;
pub const STALE_PENDING: i64 = 15 * 60;
pub const MAY_HAVE_GONE: &str = "Пост мог уйти в канал, проверьте перед повтором";

pub fn publisher_may_have_gone() -> String {
    format!("запрос прерван во время отправки. {MAY_HAVE_GONE}")
}

pub fn fingerprint(scope: &str, body: &Value) -> String {
    let mut raw = String::new();
    write_python_json(&json!([scope, body]), &mut raw);
    crate::hex::encode(&Sha256::digest(raw.as_bytes()))
}

fn write_python_json(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&python_number(value)),
        Value::String(value) => output.push_str(&Value::String(value.clone()).to_string()),
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push_str(", ");
                }
                write_python_json(item, output);
            }
            output.push(']');
        }
        Value::Object(object) => {
            output.push('{');
            let keys = object.keys().collect::<Vec<_>>();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    output.push_str(", ");
                }
                output.push_str(&Value::String((*key).clone()).to_string());
                output.push_str(": ");
                write_python_json(&object[*key], output);
            }
            output.push('}');
        }
    }
}

fn python_number(number: &serde_json::Number) -> String {
    if !number.is_f64() {
        return number.to_string();
    }
    let Some(value) = number.as_f64() else {
        return number.to_string();
    };
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_owned();
    }
    let scientific = format!("{value:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return number.to_string();
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return number.to_string();
    };
    if !(-4..16).contains(&exponent) {
        return format!(
            "{mantissa}e{}{abs:02}",
            if exponent < 0 { "-" } else { "+" },
            abs = exponent.unsigned_abs()
        );
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let digits = mantissa.trim_start_matches('-').replace('.', "");
    let point = exponent + 1;
    if point <= 0 {
        format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!(
            "{sign}{digits}{}.0",
            "0".repeat(point as usize - digits.len())
        )
    } else {
        format!(
            "{sign}{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    }
}

#[derive(Clone)]
pub struct Idempotency {
    context: AccountContext,
}

impl Idempotency {
    pub fn new(context: AccountContext) -> Self {
        Self { context }
    }

    async fn finish(&self, key: &str, status: &str, result: Value) -> Result<(), ServiceError> {
        let (key, status) = (key.to_owned(), status.to_owned());
        blocking::run(&self.context, move |context| {
            Ok(idempotency::finish(context, &key, &status, &result)?)
        })
        .await
    }

    pub async fn run<F, Fut>(
        &self,
        key: Option<&str>,
        scope: &str,
        body: &Value,
        _actor: &str,
        action: F,
    ) -> Result<Value, ServiceError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Value, ServiceError>>,
    {
        let Some(key) = key.filter(|key| !key.is_empty()) else {
            return action().await;
        };
        if key.chars().count() > MAX_KEY {
            return Err(ServiceError::BadRequest(format!(
                "Idempotency-Key длиннее {MAX_KEY} символов"
            )));
        }
        let fingerprint = fingerprint(scope, body);
        let owned_key = key.to_owned();
        let owned_scope = scope.to_owned();
        let owned_fp = fingerprint.clone();
        let existing = blocking::run(&self.context, move |context| {
            Ok(idempotency::claim(
                context,
                &owned_key,
                &owned_scope,
                &owned_fp,
            )?)
        })
        .await?;
        if let Some(existing) = existing {
            if existing["fingerprint"].as_str() != Some(fingerprint.as_str()) {
                return Err(ServiceError::Unprocessable(
                    "этот Idempotency-Key уже использован для другого запроса".to_owned(),
                ));
            }
            return match existing["status"].as_str().unwrap_or_default() {
                "done" => Ok(existing["response"].clone()),
                "uncertain" => Err(ServiceError::MayHaveGone(existing["response"].clone())),
                _ if now() - existing["created_at"].as_i64().unwrap_or_default()
                    > STALE_PENDING =>
                {
                    Err(ServiceError::MayHaveGone(json!(publisher_may_have_gone())))
                }
                _ => Err(ServiceError::Conflict(
                    "запрос с этим Idempotency-Key ещё выполняется".to_owned(),
                )),
            };
        }
        // A canceled operation leaves pending durable; replay is blocked even if the runtime dies.
        match action().await {
            Ok(result) => {
                self.finish(key, "done", result.clone()).await?;

                Ok(result)
            }
            Err(error) => {
                match &error {
                    ServiceError::MayHaveGone(detail) => {
                        self.finish(key, "uncertain", detail.clone()).await?;
                    }
                    ServiceError::Telegram(crate::telegram::TelegramError::NoAnswer(_)) => {
                        self.finish(key, "uncertain", json!(publisher_may_have_gone()))
                            .await?;
                    }
                    ServiceError::BadRequest(_)
                    | ServiceError::Unprocessable(_)
                    | ServiceError::Invalid(_)
                    | ServiceError::Conflict(_)
                    | ServiceError::NotFound(_)
                    | ServiceError::Upstream(_)
                    | ServiceError::Telegram(_) => {
                        let key = key.to_owned();
                        blocking::run(&self.context, move |context| {
                            Ok(idempotency::release(context, &key)?)
                        })
                        .await?;
                    }
                    ServiceError::Internal(_) | ServiceError::Store(_) => {
                        self.finish(key, "uncertain", json!(publisher_may_have_gone()))
                            .await?;
                    }
                }

                Err(error)
            }
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}
