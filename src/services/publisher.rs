use serde_json::{Value, json};

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
const RESERVED_PARAMS: &[&str] = &["chat_id", "rich_message", "reply_markup", "message_id"];

#[derive(Clone)]
pub struct Publisher {
    context: AccountContext,
    transport: TelegramTransport,
    updates_on: bool,
    settings: Option<crate::settings::Settings>,
}

impl Publisher {
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
    ) -> Result<(), ServiceError> {
        let saved = id.clone();
        blocking::run(&self.context, move |context| {
            Ok(posts::mark_failed(context, &id, &detail)?)
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
        for chat in chats {
            // Cancellation leaves a durable sending marker; recovery never resends it.
            let (record, _flight) = self
                .record_sending(chat, post, draft_id, kind, actor)
                .await?;
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
                        .record_failure(id, detail.clone(), uncertain.then_some("uncertain"))
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
                        return Err(store_error);
                    }
                    if chats.len() == 1 {
                        return Err(if uncertain {
                            ServiceError::MayHaveGone(json!(detail))
                        } else {
                            ServiceError::Telegram(error)
                        });
                    }
                    output.push(json!({"chat_id":chat,"ok":false,"error":detail}));
                    continue;
                }
            };
            let Some(message_id) = result["message_id"].as_i64() else {
                let detail = format!("Telegram response has no message_id. {MAY_HAVE_GONE}");
                self.record_failure(id, detail.clone(), Some("uncertain"))
                    .await?;
                if chats.len() == 1 {
                    return Err(ServiceError::MayHaveGone(json!(detail)));
                }
                output.push(json!({"chat_id":chat,"ok":false,"error":detail}));
                continue;
            };
            let record = blocking::run(&self.context, move |context| {
                Ok(posts::mark_sent(context, &id, message_id)?)
            })
            .await?;
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
                    self.record_failure(id, POLL_MAY_HAVE_GONE.to_owned(), None)
                        .await?;
                    return Err(ServiceError::MayHaveGone(json!(POLL_MAY_HAVE_GONE)));
                };
                blocking::run(&self.context, move |context| {
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
                .await
            }
            Err(error) => {
                let detail = if matches!(error, TelegramError::NoAnswer(_)) {
                    format!("{error}. {POLL_MAY_HAVE_GONE}")
                } else {
                    error.to_string()
                };
                self.record_failure(id, detail.clone(), None).await?;
                Err(ServiceError::Upstream(json!(detail)))
            }
        }
    }
}
