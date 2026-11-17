use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::account_context::AccountContext;
use crate::errors::ServiceError;
use crate::services::{
    blocking,
    idempotency::MAY_HAVE_GONE,
    kinds::{self, Kind},
};
use crate::store::posts::{self, NewPost};
use crate::telegram::{
    TelegramError,
    client::TelegramTransport,
    uploads::{Uploads, collect_uploads},
};

pub const POLL_MAY_HAVE_GONE: &str = "Опрос мог уйти в канал, проверьте перед повтором";
pub(crate) const RESERVED_PARAMS: &[&str] = &[
    "chat_id",
    "rich_message",
    "reply_markup",
    "message_id",
    "ephemeral_message_parameters",
];
const UNRECORDED: &str = "опубликован, студия не смогла записать — не публикуйте повторно";
const MAX_CONFIRMATIONS: usize = 256;

type ConfirmationKey = (PathBuf, String);
fn confirmations() -> &'static Mutex<BTreeMap<ConfirmationKey, Value>> {
    static CONFIRMATIONS: OnceLock<Mutex<BTreeMap<ConfirmationKey, Value>>> = OnceLock::new();
    CONFIRMATIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub fn unrecorded_ids(context: &AccountContext) -> Vec<String> {
    confirmations().lock().map_or_else(
        |_| Vec::new(),
        |items| {
            items
                .keys()
                .filter(|(path, _)| path == context.database_path())
                .map(|(_, id)| id.clone())
                .collect()
        },
    )
}

pub fn remember_confirmation(context: &AccountContext, id: &str, result: &Value) {
    if let Ok(mut items) = confirmations().lock() {
        let key = (context.database_path().to_owned(), id.to_owned());
        if items.len() >= MAX_CONFIRMATIONS && !items.contains_key(&key) {
            // The durable sending row and uncertain idempotency key still block
            // a resend. Keep existing confirmations rather than evicting one.
            tracing::error!(event="send.confirmation_capacity",post_id=%id,message_id=?result["message_id"],"confirmation buffer full; preserve the sending row and check Telegram before any retry");
            return;
        }
        items.insert(key, result.clone());
    }
}

/// Retry only local bookkeeping, never the Telegram request or its later steps.
pub fn write_unrecorded(context: &AccountContext) -> Result<(), ServiceError> {
    let mut items = confirmations()
        .lock()
        .map_err(|_| ServiceError::Internal("publication confirmations unavailable".to_owned()))?;
    let ids: Vec<_> = items
        .keys()
        .filter(|(path, _)| path == context.database_path())
        .cloned()
        .collect();
    for key in ids {
        let Some(result) = items.get(&key) else {
            continue;
        };
        let message_id = result["message_id"].as_i64().ok_or_else(|| {
            ServiceError::Internal("publication confirmation has no message_id".to_owned())
        })?;
        posts::mark_sent(context, &key.1, message_id)?;
        posts::update_delivery(
            context,
            &key.1,
            result["chat"]["id"].as_i64(),
            result["poll"]["id"].as_str(),
            None,
        )?;
        items.remove(&key);
    }
    Ok(())
}

#[derive(Clone)]
pub struct Publisher {
    context: AccountContext,
    transport: TelegramTransport,
    updates_on: bool,
    settings: Option<crate::settings::Settings>,
}

impl Publisher {
    fn scrub_error(&self, detail: &str) -> String {
        self.settings.as_ref().map_or_else(
            || detail.to_owned(),
            |settings| crate::logs::scrub(settings, detail),
        )
    }
    pub fn new(context: AccountContext, transport: TelegramTransport, updates_on: bool) -> Self {
        Self {
            context,
            transport,
            updates_on,
            settings: None,
        }
    }
    pub fn with_settings(mut self, settings: &crate::settings::Settings) -> Self {
        self.settings = Some(settings.clone());
        self
    }

    pub async fn notify(&self, id: &str, event: Option<&str>, detail: Option<&str>) {
        let Some(settings) = &self.settings else {
            return;
        };
        let id = id.to_owned();
        let result = async {
            let row =
                blocking::run(&self.context, move |context| Ok(posts::get(context, &id)?)).await?;
            if let Some(row) = row {
                if let Some(event) = event {
                    super::bot_notifications::record_event(
                        &self.context,
                        settings,
                        &row,
                        event,
                        detail,
                    )
                    .await?;
                } else {
                    super::bot_notifications::record(&self.context, settings, &row, detail).await?;
                }
            }
            Ok::<_, ServiceError>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(error=%error,"publication notification could not be recorded");
        }
    }

    pub fn request(
        chat_id: &Value,
        post: &Value,
        kind: &Kind,
    ) -> Result<(String, Value), ServiceError> {
        if let Some(field) = ["chat_id", "chat_ids"]
            .iter()
            .find(|field| post.get(**field).is_some())
        {
            return Err(ServiceError::BadRequest(format!(
                "поле {field} внутри поста не задаёт получателя: укажите chat_id или chat_ids рядом с post"
            )));
        }
        if let Some(key) = kind.key {
            let mut params = post[key].as_object().cloned().unwrap_or_default();
            params.insert("chat_id".to_owned(), chat_id.clone());
            return Ok((kind.method.to_owned(), Value::Object(params)));
        }
        let mut params = post["options"].as_object().cloned().unwrap_or_default();
        let reserved = params
            .keys()
            .filter(|key| RESERVED_PARAMS.contains(&key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !reserved.is_empty() {
            return Err(ServiceError::BadRequest(format!(
                "в options нельзя задавать {}",
                reserved.join(", ")
            )));
        }
        params.insert("chat_id".to_owned(), chat_id.clone());
        params.insert("rich_message".to_owned(), post["rich_message"].clone());
        if !post["reply_markup"].is_null() {
            params.insert("reply_markup".to_owned(), post["reply_markup"].clone());
        }
        Ok(("sendRichMessage".to_owned(), Value::Object(params)))
    }

    pub async fn send(
        &self,
        chat_id: &Value,
        post: &Value,
        kind: &Kind,
    ) -> Result<Value, TelegramError> {
        // Queued records may predate the current widget policy or have been restored
        // from a backup. Apply the same validation at delivery, before any network call.
        if kind.name == kinds::WIDGET {
            kinds::prepare(kind, &post[kinds::WIDGET])
                .map_err(|error| TelegramError::Policy(error.to_string()))?;
            if let Some(settings) = &self.settings {
                let (context, chat, base) = (
                    self.context.clone(),
                    chat_id.clone(),
                    settings.api_base.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    super::posts::check_widget_targets(&context, &[chat], &base)
                })
                .await
                .map_err(|_| {
                    TelegramError::Policy("проверка получателя виджета прервана".to_owned())
                })?
                .map_err(|error| TelegramError::Policy(error.to_string()))?;
            }
        }
        let (method, params) = Self::request(chat_id, post, kind)
            .map_err(|error| TelegramError::Policy(error.to_string()))?;
        let media_dir = self.context.media_dir.clone();
        let (params, uploads) = tokio::task::spawn_blocking(move || {
            let mut uploads = Uploads::default();
            let params = collect_uploads(&params, &media_dir, &mut uploads)?;
            Ok::<_, TelegramError>((params, uploads))
        })
        .await
        .map_err(|_| TelegramError::Policy("проверка файлов прервана".to_owned()))??;
        self.transport
            .call(&self.context, &method, params, uploads)
            .await
    }

    async fn record_sending(
        &self,
        chat: &Value,
        post: &Value,
        draft_id: Option<&str>,
        kind: &Kind,
        actor: &str,
    ) -> Result<(Value, super::in_flight::SendGuard), ServiceError> {
        let (chat, post, draft_id, kind, actor) = (
            chat.clone(),
            post.clone(),
            draft_id.map(str::to_owned),
            kind.name.to_owned(),
            actor.to_owned(),
        );
        blocking::run(&self.context, move |context| {
            super::in_flight::record(context, || {
                Ok(posts::add(
                    context,
                    NewPost {
                        chat_id: &chat,
                        payload: &post,
                        draft_id: draft_id.as_deref(),
                        status: "sending",
                        message_id: None,
                        scheduled_at: None,
                        error: None,
                        kind: &kind,
                        actor: &actor,
                    },
                )?)
            })
        })
        .await
    }

    async fn record_failure(
        &self,
        id: String,
        detail: String,
        event: Option<&str>,
        uncertain: bool,
    ) -> Result<(), ServiceError> {
        let saved = id.clone();
        let detail = self.scrub_error(&detail);
        blocking::run(&self.context, move |context| {
            if uncertain {
                posts::mark_outcome_unknown(context, &id, &detail)?;
            } else {
                posts::mark_failed(context, &id, &detail)?;
            }
            Ok(())
        })
        .await?;
        self.notify(&saved, event, None).await;
        Ok(())
    }

    pub async fn publish(
        &self,
        chats: &[Value],
        post: &Value,
        draft_id: Option<&str>,
        kind: &Kind,
        actor: &str,
    ) -> Result<(Vec<Value>, Vec<String>), ServiceError> {
        let mut output = Vec::new();
        let mut warnings = Vec::new();
        if let Err(error) = blocking::run(&self.context, write_unrecorded).await {
            tracing::warn!(error=%error,"confirmed publications still await local bookkeeping");
            if !unrecorded_ids(&self.context).is_empty() {
                return Err(ServiceError::Conflict("Предыдущие посты уже в канале, но их запись ещё не сохранена. Освободите место или восстановите доступ к базе; новую отправку студия не начинала.".to_owned()));
            }
        }
        for chat in chats {
            // Cancellation leaves a durable sending marker; recovery never resends it.
            let sending = self.record_sending(chat, post, draft_id, kind, actor).await;
            let (record, _flight) = match sending {
                Ok(record) => record,
                Err(error)
                    if output
                        .iter()
                        .any(|row: &Value| row["ok"] == true || row["outcome_unknown"] == true) =>
                {
                    output.push(json!({"chat_id":chat,"ok":false,"error":format!("не отправлено: студия не смогла записать строку поста ({error})")}));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let id = record["id"]
                .as_str()
                .ok_or_else(|| ServiceError::Internal("пост без id".to_owned()))?
                .to_owned();
            let result = match self.send(chat, post, kind).await {
                Ok(result) => result,
                Err(error) => {
                    let uncertain = matches!(error, TelegramError::NoAnswer(_));
                    let detail = if uncertain {
                        format!("{error}. {MAY_HAVE_GONE}")
                    } else {
                        error.to_string()
                    };
                    let detail = self.scrub_error(&detail);
                    if chats.len() == 1
                        && matches!(
                            error,
                            TelegramError::Policy(_) | TelegramError::MissingUpload(_)
                        )
                    {
                        blocking::run(&self.context, move |context| {
                            Ok(posts::delete(context, &id)?)
                        })
                        .await?;
                        return Err(ServiceError::Telegram(error));
                    }
                    let recorded = self
                        .record_failure(
                            id,
                            detail.clone(),
                            uncertain.then_some("uncertain"),
                            uncertain,
                        )
                        .await;
                    if let Err(store_error) = recorded {
                        if chats.len() == 1 {
                            // The refusal from Telegram is what the person needs to see; the
                            // recovery marker still records the row on the next start.
                            return Err(if uncertain {
                                ServiceError::MayHaveGone(json!(detail))
                            } else {
                                ServiceError::Telegram(error)
                            });
                        }
                        warnings.push(format!("{chat}: не удалось записать отказ: {store_error}"));
                    }
                    if chats.len() == 1 {
                        return Err(if uncertain {
                            ServiceError::MayHaveGone(json!(detail))
                        } else {
                            ServiceError::Telegram(error)
                        });
                    }
                    output.push(json!({"chat_id":chat,"ok":false,"error":detail,"outcome_unknown":uncertain}));
                    continue;
                }
            };
            let Some(message_id) = result["message_id"].as_i64() else {
                let detail = format!("Telegram response has no message_id. {MAY_HAVE_GONE}");
                if let Err(error) = self
                    .record_failure(id, detail.clone(), Some("uncertain"), true)
                    .await
                {
                    warnings.push(format!(
                        "{chat}: не удалось записать неизвестный исход: {error}"
                    ));
                }
                if chats.len() == 1 {
                    return Err(ServiceError::MayHaveGone(json!(detail)));
                }
                output
                    .push(json!({"chat_id":chat,"ok":false,"error":detail,"outcome_unknown":true}));
                continue;
            };
            let saved_id = id.clone();
            let saved = blocking::run(&self.context, move |context| {
                Ok(posts::mark_sent(context, &saved_id, message_id)?)
            })
            .await;
            let record = match saved {
                Ok(record) => record,
                Err(error) => {
                    remember_confirmation(&self.context, &id, &result);
                    tracing::error!(event="send.unrecorded",post_id=%id,chat_id=%chat,message_id,error=%error,"publication confirmed by Telegram but local record failed");
                    warnings.push(format!(
                        "{chat}: пост {UNRECORDED} (message_id {message_id}): {error}"
                    ));
                    output.push(json!({"chat_id":chat,"ok":true,"unrecorded":true,"published":true,"post_id":id,"message_id":message_id,"message":result,"error":error.to_string()}));
                    continue;
                }
            };
            let mut after_warnings = Vec::new();
            if let Some(draft_id) = draft_id {
                let (chat, draft_id) = (chat.clone(), draft_id.to_owned());
                if let Err(error) = blocking::run(&self.context, move |context| {
                    Ok(posts::drop_refused(
                        context,
                        &chat,
                        &draft_id,
                        &[
                            MAY_HAVE_GONE,
                            POLL_MAY_HAVE_GONE,
                            crate::store::scheduler::INTERRUPTED_SEND,
                        ],
                    )?)
                })
                .await
                {
                    after_warnings.push(format!("не удалось очистить предыдущие отказы: {error}"));
                }
            }
            match self.after_send(&record, post, &result, actor, kind).await {
                Ok(items) => after_warnings.extend(items),
                Err(error) => after_warnings.push(error.to_string()),
            }
            let id = record["id"].as_str().unwrap_or_default().to_owned();
            let record =
                match blocking::run(&self.context, move |context| Ok(posts::get(context, &id)?))
                    .await
                {
                    Ok(refreshed) => refreshed.unwrap_or(record),
                    Err(error) => {
                        after_warnings
                            .push(format!("не удалось перечитать отправленный пост: {error}"));
                        record
                    }
                };
            self.notify(
                record["id"].as_str().unwrap_or_default(),
                None,
                Some(&after_warnings.join("; ")),
            )
            .await;
            warnings.extend(after_warnings.into_iter().map(|warning| {
                if chats.len() > 1 {
                    let chat = chat
                        .as_str()
                        .map_or_else(|| chat.to_string(), str::to_owned);
                    format!("{chat}: {warning}")
                } else {
                    warning
                }
            }));
            output.push(json!({"chat_id":chat,"ok":true,"post":record,"message":result}));
        }
        if let Some(settings) = &self.settings {
            for warning in &mut warnings {
                *warning = crate::logs::scrub(settings, warning);
            }
            for row in &mut output {
                if let Some(error) = row["error"].as_str() {
                    row["error"] = json!(crate::logs::scrub(settings, error));
                }
            }
        }
        if output.iter().any(|row| row["unrecorded"] == true) {
            return Err(ServiceError::MayHaveGone(json!({
                "message":format!("Пост {UNRECORDED}. Запись будет дописана, когда база снова принимает изменения; шаги после отправки пропущены."),
                "published":true,"unrecorded":true,"results":output,"warnings":warnings,
            })));
        }
        Ok((output, warnings))
    }

    pub async fn after_send(
        &self,
        record: &Value,
        post: &Value,
        result: &Value,
        actor: &str,
        kind: &Kind,
    ) -> Result<Vec<String>, ServiceError> {
        let id = record["id"].as_str().unwrap_or_default().to_owned();
        let mut warnings = Vec::new();
        let comment_status = if post["comment"].is_null() {
            None
        } else if self.updates_on {
            Some("waiting")
        } else {
            warnings.push(
                "первый комментарий не будет опубликован: приём событий выключен (TG_UPDATES=off)"
                    .to_owned(),
            );
            Some("skipped")
        };
        let delivery_id = id.clone();
        let chat_num = result["chat"]["id"].as_i64();
        let poll_id = result["poll"]["id"].as_str().map(str::to_owned);
        blocking::run(&self.context, move |context| {
            Ok(posts::update_delivery(
                context,
                &delivery_id,
                chat_num,
                poll_id.as_deref(),
                comment_status,
            )?)
        })
        .await?;
        if post["pin"].as_bool() == Some(true) {
            let notification = post["options"]["disable_notification"]
                .as_bool()
                .unwrap_or(true);
            match self.transport.call(&self.context, "pinChatMessage", json!({
                "chat_id":result["chat"]["id"],"message_id":result["message_id"],"disable_notification":notification,
            }), Uploads::default()).await {
                Ok(_) => blocking::run(&self.context, move |context| Ok(posts::set_pinned(context, &id, true)?)).await?,
                Err(error) => warnings.push(format!("не удалось закрепить: {error}")),
            }
        }
        if !post["poll_after"].is_null()
            && kind.name == kinds::RICH
            && let Err(error) = self.send_poll_after(record, post, result, actor).await
        {
            warnings.push(format!("опрос после поста: {error}"));
        }
        Ok(warnings)
    }

    async fn send_poll_after(
        &self,
        record: &Value,
        post: &Value,
        result: &Value,
        actor: &str,
    ) -> Result<(), ServiceError> {
        let kind = kinds::get(Some(kinds::POLL))?;
        let mut poll = kinds::prepare(kind, &post["poll_after"])?;
        if let Some(options) = post["options"].as_object() {
            for key in ["message_thread_id", "disable_notification"] {
                if let Some(value) = options.get(key)
                    && poll.get(key).is_none()
                {
                    poll[key] = value.clone();
                }
            }
        }
        let sent = json!({"poll":poll});
        let chat = &result["chat"]["id"];
        let (poll_record, _flight) = self
            .record_sending(chat, &sent, record["draft_id"].as_str(), kind, actor)
            .await?;
        let id = poll_record["id"]
            .as_str()
            .ok_or_else(|| ServiceError::Internal("опрос без id".to_owned()))?
            .to_owned();
        match self.send(chat, &sent, kind).await {
            Ok(poll_result) => {
                let Some(message_id) = poll_result["message_id"].as_i64() else {
                    self.record_failure(id, POLL_MAY_HAVE_GONE.to_owned(), None, true)
                        .await?;
                    return Err(ServiceError::MayHaveGone(json!(POLL_MAY_HAVE_GONE)));
                };
                let confirmation = poll_result.clone();
                let saved_id = id.clone();
                let saved = blocking::run(&self.context, move |context| {
                    posts::mark_sent(context, &id, message_id)?;
                    posts::update_delivery(
                        context,
                        &id,
                        poll_result["chat"]["id"].as_i64(),
                        poll_result["poll"]["id"].as_str(),
                        None,
                    )?;
                    Ok(())
                })
                .await;
                if let Err(error) = saved {
                    remember_confirmation(&self.context, &saved_id, &confirmation);
                    tracing::error!(event="send.unrecorded",post_id=%saved_id,message_id,error=%error,"poll confirmed by Telegram but local record failed");
                    return Err(ServiceError::MayHaveGone(
                        json!({"message":format!("опрос {UNRECORDED}"),"published":true,"message_id":message_id,"post_id":saved_id}),
                    ));
                }
                Ok(())
            }
            Err(error) => {
                let detail = if matches!(error, TelegramError::NoAnswer(_)) {
                    format!("{error}. {POLL_MAY_HAVE_GONE}")
                } else {
                    error.to_string()
                };
                self.record_failure(
                    id,
                    detail.clone(),
                    None,
                    matches!(error, TelegramError::NoAnswer(_)),
                )
                .await?;
                Err(ServiceError::Upstream(json!(detail)))
            }
        }
    }
}
