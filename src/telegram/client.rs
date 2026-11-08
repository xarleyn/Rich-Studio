use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use reqwest::multipart::{Form, Part};
use serde_json::{Map, Value, json};

use crate::account_context::AccountContext;
use crate::store::channels;
use crate::telegram::TelegramError;
use crate::telegram::uploads::Uploads;

const MAX_RETRY_AFTER: u64 = 30;
const MAX_RETRIES: usize = 3;
const READ_METHODS: &[&str] = &[
    "getMe",
    "getChat",
    "getChatMember",
    "answerCallbackQuery",
    // Custom emoji packs: a set by short name and sticker files, no chat involved.
    "getStickerSet",
    "getCustomEmojiStickers",
    "getFile",
];

#[derive(Clone)]
pub struct TelegramTransport {
    api_base: String,
    token: String,
    metrics: Option<Arc<crate::metrics::Metrics>>,
    logs: Option<Arc<crate::logs::Logger>>,
}

impl TelegramTransport {
    /// Check a linked destination at the final event boundary, before touching account data.
    pub async fn check_account_member(
        &self,
        context: &AccountContext,
        chat: &Value,
    ) -> Result<(), TelegramError> {
        if context.account_id.is_none() {
            return Ok(());
        }
        let copy = context.clone();
        let chat = chat.clone();
        let transport = self.clone();
        let target = tokio::task::spawn_blocking(move || transport.linked_target(&copy, &chat))
            .await
            .map_err(|_| TelegramError::Policy("account check interrupted".to_owned()))??;
        if target.as_i64() == context.account_id {
            return Ok(());
        }
        self.check_member(context, &target, None).await
    }

    /// Only studio-owned private text to the account's own Telegram identity.
    pub async fn send_studio_private(
        &self,
        context: &AccountContext,
        uid: i64,
        text: String,
        markup: Value,
    ) -> Result<Value, TelegramError> {
        if uid <= 0
            || uid >= (1_i64 << 52)
            || context.account_id.is_some_and(|account| account != uid)
        {
            return Err(TelegramError::Policy(
                "Получатель сообщения не совпадает с текущей учёткой.".to_owned(),
            ));
        }
        self.call_authorized("sendMessage", json!({"chat_id":uid,"text":text,"parse_mode":"HTML","reply_markup":markup,"link_preview_options":{"is_disabled":true}}), Uploads::default()).await
    }

    /// The sole discussion exception: one reply to a verified source-channel forward.
    pub async fn send_linked_comment(
        &self,
        context: &AccountContext,
        source: &Value,
        discussion: &Value,
        message_id: i64,
        comment: Value,
    ) -> Result<Value, TelegramError> {
        let values = json!({"chat_id":discussion,"rich_message":comment,"reply_parameters":{"message_id":message_id}});
        if context.account_id.is_some() {
            if paid_or_business(&values) {
                return Err(TelegramError::Policy(
                    "Платная рассылка и бизнес-подключения здесь недоступны.".to_owned(),
                ));
            }
            let copy = context.clone();
            let source = source.clone();
            let transport = self.clone();
            let source =
                tokio::task::spawn_blocking(move || transport.linked_target(&copy, &source))
                    .await
                    .map_err(|_| TelegramError::Policy("account check interrupted".to_owned()))??;
            self.check_member(context, &source, Some("sendLinkedComment"))
                .await?;
            let current = self
                .call_authorized("getChat", json!({"chat_id":source}), Uploads::default())
                .await?;
            if current["id"] != source
                || current["type"].as_str() != Some("channel")
                || current["linked_chat_id"] != *discussion
            {
                return Err(TelegramError::Policy(
                    "Группа больше не является обсуждением исходного канала.".to_owned(),
                ));
            }
        }
        let media_dir = context.media_dir.clone();
        let (values, uploads) = tokio::task::spawn_blocking(move || {
            let mut uploads = Uploads::default();
            let values =
                crate::telegram::uploads::collect_uploads(&values, &media_dir, &mut uploads)?;
            Ok::<_, TelegramError>((values, uploads))
        })
        .await
        .map_err(|_| TelegramError::Policy("media worker interrupted".to_owned()))??;
        self.call_authorized("sendRichMessage", values, uploads)
            .await
    }

    pub fn new(api_base: String, token: String) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
            metrics: None,
            logs: None,
        }
    }

    pub fn with_observability(
        mut self,
        metrics: Arc<crate::metrics::Metrics>,
        logs: Arc<crate::logs::Logger>,
    ) -> Self {
        self.metrics = Some(metrics);
        self.logs = Some(logs);
        self
    }

    pub async fn call(
        &self,
        context: &AccountContext,
        method: &str,
        payload: Value,
        uploads: Uploads,
    ) -> Result<Value, TelegramError> {
        let payload = self.authorize(context, method, payload).await?;
        self.call_authorized(method, payload, uploads).await
    }

    /// `getFile` plus the byte download from `{api_base}/file/bot{token}/{file_path}`.
    /// The URL carries the bot token, so it is never logged or copied into an error.
    /// Returns the bytes and the `file_path` `getFile` reported.
    pub async fn download_file(
        &self,
        context: &AccountContext,
        file_id: &str,
    ) -> Result<(Vec<u8>, String), TelegramError> {
        let file = self
            .call(
                context,
                "getFile",
                json!({"file_id": file_id}),
                Uploads::default(),
            )
            .await?;
        let path = file
            .get("file_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| TelegramError::NoAnswer("getFile не вернул file_path".to_owned()))?;
        let url = format!("{}/file/bot{}/{}", self.api_base, self.token, path);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                TelegramError::Unreachable("нет связи с Telegram: файл не скачан".to_owned())
            })?;
        let response = client.get(url).send().await.map_err(|error| {
            if error.is_connect() {
                TelegramError::Unreachable("нет связи с Telegram: файл не скачан".to_owned())
            } else {
                TelegramError::NoAnswer("Telegram не ответил: файл не скачан".to_owned())
            }
        })?;
        if !response.status().is_success() {
            return Err(TelegramError::NoAnswer(format!(
                "скачивание файла: HTTP {}",
                response.status().as_u16()
            )));
        }
        let data = response.bytes().await.map_err(|_| {
            TelegramError::NoAnswer("Telegram оборвал ответ: файл не скачан".to_owned())
        })?;
        Ok((data.to_vec(), path.to_owned()))
    }

    async fn call_authorized(
        &self,
        method: &str,
        payload: Value,
        uploads: Uploads,
    ) -> Result<Value, TelegramError> {
        let started = Instant::now();
        let chat = loggable_chat(payload.get("chat_id"));
        let result = self.call_inner(method, payload, uploads).await;
        let quiet = matches!(method, "getUpdates" | "getMe" | "answerCallbackQuery");
        let error = result.as_ref().err().map(ToString::to_string);
        let code = match &result {
            Err(TelegramError::Refused { code, .. }) => *code,
            Err(TelegramError::RateLimited { .. }) => Some(429),
            _ => None,
        };
        let ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if let Some(metrics) = &self.metrics {
            metrics.telegram_call(method, result.is_ok(), error.as_deref(), ms, code, quiet);
        }
        if let Some(logs) = &self.logs {
            match &error {
                Some(error) => logs.event(
                    "WARNING",
                    "rich-studio.telegram",
                    error,
                    json!({"event":"telegram.error","method":method,"code":code,"ms":ms,"chat_id":chat}),
                ),
                None if !quiet => logs.event(
                    "INFO",
                    "rich-studio.telegram",
                    &format!("{method} ok"),
                    json!({"event":"telegram.call","method":method,"ms":ms,"chat_id":chat}),
                ),
                None => {}
            }
        }
        result
    }

    async fn call_inner(
        &self,
        method: &str,
        payload: Value,
        uploads: Uploads,
    ) -> Result<Value, TelegramError> {
        if self.token.is_empty() {
            return Err(TelegramError::Refused {
                description: "TG_BOT_TOKEN не задан (см. .env)".to_owned(),
                code: None,
                raw: None,
            });
        }
        for attempt in 0..=MAX_RETRIES {
            match self.post(method, &payload, &uploads).await {
                Err(TelegramError::RateLimited {
                    retry_after: Some(wait),
                    ..
                }) if wait > 0 && wait <= MAX_RETRY_AFTER && attempt < MAX_RETRIES => {
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                result => return result,
            }
        }
        Err(TelegramError::Unreachable(
            "Telegram retry loop ended unexpectedly".to_owned(),
        ))
    }

    async fn post(
        &self,
        method: &str,
        payload: &Value,
        uploads: &Uploads,
    ) -> Result<Value, TelegramError> {
        let url = format!("{}/bot{}/{}", self.api_base, self.token, method);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                TelegramError::Unreachable("нет связи с Telegram: запрос не отправлен".to_owned())
            })?;
        let params = payload
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .collect::<Map<_, _>>();
        let request = if uploads.files.is_empty() {
            client.post(url).json(&Value::Object(params))
        } else {
            let mut form = Form::new();
            for (key, value) in params {
                let text = match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                };
                form = form.text(key, text);
            }
            for (key, path) in &uploads.files {
                let data = tokio::fs::read(path).await.map_err(|_| {
                    TelegramError::MissingUpload(format!(
                        "файл {:?} не найден в загрузках",
                        path.file_name().unwrap_or_default()
                    ))
                })?;
                let part = Part::bytes(data).file_name(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                );
                form = form.part(key.clone(), part);
            }
            client.post(url).multipart(form)
        };
        let response = request.send().await.map_err(|error| {
            if error.is_connect() {
                TelegramError::Unreachable("нет связи с Telegram: запрос не отправлен".to_owned())
            } else {
                TelegramError::NoAnswer(
                    "Telegram не ответил: результат запроса неизвестен".to_owned(),
                )
            }
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|_| {
            TelegramError::NoAnswer(
                "Telegram оборвал ответ: результат запроса неизвестен".to_owned(),
            )
        })?;
        let body = serde_json::from_str::<Value>(&text).ok();
        let detail = body
            .as_ref()
            .and_then(|body| body.get("description"))
            .and_then(Value::as_str)
            .map(|text| text.replace(&self.token, "<token>"))
            .unwrap_or_else(|| {
                text.replace(&self.token, "<token>")
                    .chars()
                    .take(300)
                    .collect()
            });
        if status.is_server_error()
            || body
                .as_ref()
                .and_then(|body| body.get("error_code"))
                .and_then(Value::as_i64)
                .is_some_and(|code| code >= 500)
        {
            return Err(TelegramError::NoAnswer(format!(
                "Telegram ответил HTTP {}: {detail}",
                status.as_u16()
            )));
        }
        let Some(body) = body.and_then(|body| body.as_object().cloned()) else {
            return Err(TelegramError::NoAnswer(format!(
                "Telegram вернул неразбираемый ответ HTTP {}: {detail}",
                status.as_u16()
            )));
        };
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = body.get("error_code").and_then(Value::as_i64);
            let raw = Some(redact(Value::Object(body), &self.token));
            if code == Some(429) {
                let retry_after = raw
                    .as_ref()
                    .and_then(|body| body.get("parameters"))
                    .and_then(|parameters| parameters.get("retry_after"))
                    .and_then(Value::as_u64);
                return Err(TelegramError::RateLimited {
                    description: detail,
                    retry_after,
                    raw,
                });
            }
            return Err(TelegramError::Refused {
                description: detail,
                code,
                raw,
            });
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn authorize(
        &self,
        context: &AccountContext,
        method: &str,
        mut payload: Value,
    ) -> Result<Value, TelegramError> {
        let Some(uid) = context.account_id else {
            return Ok(payload);
        };
        if READ_METHODS.contains(&method) {
            return Ok(payload);
        }
        if paid_or_business(&payload) {
            return Err(TelegramError::Policy(
                "Платная массовая рассылка и бизнес-подключения здесь недоступны.".to_owned(),
            ));
        }
        let chat = payload.get("chat_id").cloned().ok_or_else(|| {
            TelegramError::Policy("Этот метод недоступен для пользовательской учётки.".to_owned())
        })?;
        let target = self.linked_target_async(context, chat).await?;
        if target.as_i64() == Some(uid) {
            if ![
                "sendRichMessage",
                "sendPoll",
                "sendLivePhoto",
                "sendPaidMedia",
                "sendWidget",
                "editMessageWidgetData",
            ]
            .contains(&method)
            {
                return Err(TelegramError::Policy(
                    "В личном чате доступен только предпросмотр.".to_owned(),
                ));
            }
        } else {
            self.check_member(context, &target, Some(method)).await?;
        }
        if let Some(reply) = payload.get("reply_parameters").cloned() {
            let reply_object = reply.as_object().ok_or_else(|| {
                TelegramError::Policy("reply_parameters задаётся объектом JSON.".to_owned())
            })?;
            let mut reply_object = reply_object.clone();
            if let Some(source) = reply_object.get("chat_id").cloned() {
                let source = self.linked_target_async(context, source).await?;
                if source != target && source.as_i64() != Some(uid) {
                    self.check_member(context, &source, None).await?;
                }
                reply_object.insert("chat_id".to_owned(), source);
            }
            payload["reply_parameters"] = Value::Object(reply_object);
        }
        payload["chat_id"] = target;
        Ok(payload)
    }

    pub fn linked_target(
        &self,
        context: &AccountContext,
        chat: &Value,
    ) -> Result<Value, TelegramError> {
        let Some(uid) = context.account_id else {
            return Ok(chat.clone());
        };
        if chat.as_i64() == Some(uid) || chat.as_str().is_some_and(|value| value == uid.to_string())
        {
            return Ok(json!(uid));
        }
        let channel = channels::get(context, chat)
            .map_err(|error| TelegramError::Policy(error.to_string()))?
            .ok_or_else(|| {
                TelegramError::Policy(
                    "Сначала привяжите этот канал к своей учётке в окне «Каналы».".to_owned(),
                )
            })?;
        channel
            .get("chat_num")
            .cloned()
            .filter(|value| !value.is_null())
            .ok_or_else(|| {
                TelegramError::Policy(
                    "Сначала привяжите этот канал к своей учётке в окне «Каналы».".to_owned(),
                )
            })
    }

    async fn linked_target_async(
        &self,
        context: &AccountContext,
        chat: Value,
    ) -> Result<Value, TelegramError> {
        let context = context.clone();
        let transport = self.clone();
        tokio::task::spawn_blocking(move || transport.linked_target(&context, &chat))
            .await
            .map_err(|_| TelegramError::Policy("account worker interrupted".to_owned()))?
    }

    async fn check_member(
        &self,
        context: &AccountContext,
        target: &Value,
        method: Option<&str>,
    ) -> Result<(), TelegramError> {
        let uid = context.account_id.unwrap_or_default();
        let member = self
            .call_authorized(
                "getChatMember",
                json!({"chat_id":target,"user_id":uid}),
                Uploads::default(),
            )
            .await?;
        let status = member
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let copy = context.clone();
        let target_copy = target.clone();
        let channel = tokio::task::spawn_blocking(move || channels::get(&copy, &target_copy))
            .await
            .map_err(|_| TelegramError::Policy("account check interrupted".to_owned()))?
            .map_err(|error| TelegramError::Policy(error.to_string()))?;
        let chat_type = channel
            .as_ref()
            .and_then(|channel| channel.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let allowed = status == "creator"
            || status == "administrator"
                && ((chat_type != "channel" && method != Some("sendLinkedComment"))
                    || member.get("can_post_messages").and_then(Value::as_bool) == Some(true));
        if !allowed {
            return Err(TelegramError::Policy(
                "Нужны ваши права администратора с публикацией сообщений в этом канале.".to_owned(),
            ));
        }
        let required = match method {
            Some(method) if method.contains("InviteLink") => Some("can_invite_users"),
            Some("deleteMessage" | "deleteMessages") => Some("can_delete_messages"),
            Some("pinChatMessage" | "unpinChatMessage" | "unpinAllChatMessages") => {
                Some(if chat_type == "channel" {
                    "can_edit_messages"
                } else {
                    "can_pin_messages"
                })
            }
            Some(method)
                if chat_type == "channel"
                    && (method.starts_with("editMessage") || method == "stopPoll") =>
            {
                Some("can_edit_messages")
            }
            _ => None,
        };
        if status != "creator"
            && required
                .is_some_and(|right| member.get(right).and_then(Value::as_bool) != Some(true))
        {
            return Err(TelegramError::Policy(format!(
                "У вас нет права Telegram {} для этой операции.",
                required.unwrap_or_default()
            )));
        }
        Ok(())
    }
}

/// A raw call may put post text into chat_id: only an id or a short name may reach the log.
fn loggable_chat(chat: Option<&Value>) -> Value {
    match chat {
        Some(Value::String(text)) if text.chars().count() <= 64 => json!(text),
        Some(Value::Number(_)) => chat.cloned().unwrap_or(Value::Null),
        Some(Value::Null) | None => Value::Null,
        Some(_) => json!("?"),
    }
}

fn redact(value: Value, token: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(token, "<token>")),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|item| redact(item, token)).collect())
        }
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| (key.replace(token, "<token>"), redact(value, token)))
                .collect(),
        ),
        other => other,
    }
}

fn paid_or_business(value: &Value) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            (key == "allow_paid_broadcast" && value.as_bool() == Some(true))
                || (key == "business_connection_id" && !value.is_null())
                || paid_or_business(value)
        }),
        Value::Array(items) => items.iter().any(paid_or_business),
        _ => false,
    }
}
