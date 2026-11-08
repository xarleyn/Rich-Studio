use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Iso8601;

use crate::account_context::AccountContext;
use crate::errors::ServiceError;
use crate::services::idempotency::MAY_HAVE_GONE;
use crate::services::kinds::{self, Kind};
use crate::services::publisher::Publisher;
use crate::settings::Settings;
use crate::spec::source::Spec;
use crate::spec::validator::Validator;
use crate::store::{drafts, posts};
use crate::telegram::TelegramError;
use crate::telegram::client::TelegramTransport;
use crate::telegram::uploads::{Uploads, collect_uploads};

#[derive(Clone)]
pub struct PostService {
    context: AccountContext,
    transport: TelegramTransport,
    settings: Settings,
    spec: Spec,
}

pub async fn from_state(state: &crate::AppState) -> PostService {
    PostService::new(
        state.account.clone(),
        state.transport(),
        &state.settings,
        state.spec.read().await.clone(),
    )
}

impl PostService {
    pub fn new(
        context: AccountContext,
        transport: TelegramTransport,
        settings: &Settings,
        spec: Spec,
    ) -> Self {
        Self {
            context,
            transport,
            settings: settings.clone(),
            spec,
        }
    }

    fn publisher(&self) -> Publisher {
        Publisher::new(
            self.context.clone(),
            self.transport.clone(),
            self.settings.updates_on,
        )
        .with_settings(&self.settings)
    }

    pub async fn run<T, F>(&self, action: F) -> Result<T, ServiceError>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> Result<T, ServiceError> + Send + 'static,
    {
        let service = self.clone();
        tokio::task::spawn_blocking(move || action(service))
            .await
            .map_err(|_| ServiceError::Internal("операция поста прервана".into()))?
    }

    pub fn validate(&self, post: &Value) -> Result<Value, ServiceError> {
        let kind = kinds::get(Some(kinds::RICH))?;
        let _ = Publisher::request(&Value::Null, post, kind)?;
        let validator = Validator::new(&self.spec);
        let mut report = validator.check_post(post);
        if report["ok"].as_bool() != Some(true) {
            report["message"] = json!("пост не прошёл проверку");
            return Err(ServiceError::Invalid(report));
        }
        if !post["comment"].is_null() {
            let comment = validator.check_post(&json!({"rich_message":post["comment"]}));
            if comment["ok"].as_bool() != Some(true) {
                return Err(ServiceError::Invalid(
                    json!({"message":"первый комментарий не прошёл проверку","ok":false,"errors":comment["errors"],"warnings":report["warnings"]}),
                ));
            }
        }
        if !post["poll_after"].is_null() {
            let kind = kinds::get(Some(kinds::POLL))?;
            if let Err(error) = kinds::prepare(kind, &post["poll_after"]) {
                return Err(match error {
                    ServiceError::BadRequest(message) => ServiceError::Invalid(json!({
                        "message": "опрос после поста не прошёл проверку",
                        "ok": false,
                        "errors": [message],
                        "warnings": [],
                    })),
                    other => other,
                });
            }
        }
        Ok(report)
    }

    pub fn resolve_chat(&self, chat_id: &Value) -> Result<Value, ServiceError> {
        let mut chat = match chat_id {
            Value::String(value) => Value::String(value.trim().to_owned()),
            other => other.clone(),
        };
        if chat.is_null() || chat.as_str() == Some("") {
            chat = Value::String(self.settings.default_chat.clone());
        }
        if chat.is_null() || chat.as_str() == Some("") {
            return Err(ServiceError::BadRequest(
                "не указан chat_id и не задан TG_DEFAULT_CHAT".to_owned(),
            ));
        }
        if let Some(text) = chat
            .as_str()
            .and_then(|text| text.parse::<i64>().ok().map(|_| text.to_owned()))
        {
            chat = json!(
                text.parse::<i64>()
                    .map_err(|error| ServiceError::BadRequest(error.to_string()))?
            );
        }
        self.transport
            .linked_target(&self.context, &chat)
            .map_err(Into::into)
    }

    pub fn targets(
        &self,
        chat_id: &Value,
        chat_ids: &Value,
        draft_id: Option<&str>,
    ) -> Result<Vec<Value>, ServiceError> {
        let mut inherited = None;
        if chat_ids.as_array().is_none_or(Vec::is_empty)
            && (chat_id.is_null() || chat_id.as_str() == Some(""))
            && let Some(draft_id) = draft_id
        {
            let draft = drafts::get(&self.context, draft_id)?.ok_or_else(|| {
                ServiceError::BadRequest(format!("черновика {draft_id} нет: укажите chat_id явно"))
            })?;
            inherited = draft["chat_ids"].as_array().cloned();
        }
        let source = chat_ids
            .as_array()
            .filter(|items| !items.is_empty())
            .cloned()
            .or(inherited)
            .unwrap_or_default();
        let mut chats = Vec::new();
        if source.is_empty() {
            chats.push(self.resolve_chat(chat_id)?);
        } else {
            let pattern = Regex::new(r"^@\w+$|^-?\d+$")
                .map_err(|error| ServiceError::Internal(error.to_string()))?;
            let mut normalized = Vec::<String>::new();
            for raw in source {
                let text = match &raw {
                    Value::String(text) => text.trim().to_owned(),
                    other => other.to_string(),
                };
                if text.is_empty() {
                    return Err(ServiceError::BadRequest(
                        "в списке каналов пустой элемент: уберите его или впишите канал".to_owned(),
                    ));
                }
                if !pattern.is_match(&text) {
                    return Err(ServiceError::BadRequest(format!(
                        "канал {text:?}: нужен @username или числовой id (-100…)"
                    )));
                }
                if !normalized
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(&text))
                {
                    normalized.push(text.clone());
                    let resolved = self.resolve_chat(&Value::String(text))?;
                    if !chats.contains(&resolved) {
                        chats.push(resolved);
                    }
                }
            }
        }
        Ok(chats)
    }

    fn check_thread(&self, chats: &[Value], post: &Value) -> Result<(), ServiceError> {
        let mut thread = post["options"]["message_thread_id"].clone();
        if thread.is_null() || thread.as_str() == Some("") {
            thread = post["poll_after"]["message_thread_id"].clone();
        }
        if thread.is_null() || thread.as_str() == Some("") {
            return Ok(());
        }
        if chats.len() > 1 {
            return Err(ServiceError::BadRequest(
                "тема форума (message_thread_id) задаётся для одного канала, а каналов несколько"
                    .to_owned(),
            ));
        }
        if let Some(channel) = crate::store::channels::get(&self.context, &chats[0])?
            && !channel["type"].is_null()
            && channel["is_forum"].as_i64() == Some(0)
        {
            return Err(ServiceError::BadRequest(format!(
                "{} не форум: уберите message_thread_id из options",
                channel["title"].as_str().unwrap_or_default()
            )));
        }
        Ok(())
    }

    fn not_an_edit_copy(&self, draft_id: Option<&str>, as_new: bool) -> Result<(), ServiceError> {
        if as_new {
            return Ok(());
        }
        if let Some(draft_id) = draft_id
            && let Some(draft) = drafts::get(&self.context, draft_id)?
            && let Some(post_id) = draft["post_id"].as_str()
        {
            return Err(ServiceError::Conflict(format!(
                "черновик {draft_id} — правка поста {post_id}: замените пост (POST /api/posts/{post_id}/edit, MCP edit_post) или передайте as_new: true, чтобы опубликовать копию новым постом"
            )));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // Mirrors the shared REST/MCP publish contract.
    pub async fn send(
        &self,
        post: &Value,
        chat_id: &Value,
        chat_ids: &Value,
        draft_id: Option<&str>,
        actor: &str,
        dry_run: bool,
        as_new: bool,
    ) -> Result<Value, ServiceError> {
        let local_post = post.clone();
        let local_chat = chat_id.clone();
        let local_chats = chat_ids.clone();
        let local_draft = draft_id.map(str::to_owned);
        let (chats, report, dry_report) = self
            .run(move |service| {
                service.not_an_edit_copy(local_draft.as_deref(), as_new)?;
                let report = service.validate(&local_post)?;
                let chats = service.targets(&local_chat, &local_chats, local_draft.as_deref())?;
                service.check_thread(&chats, &local_post)?;
                let dry_report = if dry_run {
                    Some(service.dry_run_report(&chats, &local_post, report["warnings"].clone())?)
                } else {
                    None
                };
                Ok((chats, report, dry_report))
            })
            .await?;
        if let Some(report) = dry_report {
            return Ok(report);
        }
        let (mut results, warnings) = self
            .publisher()
            .publish(
                &chats,
                post,
                draft_id,
                kinds::get(Some(kinds::RICH))?,
                actor,
            )
            .await?;
        titled_results(&self.context, &mut results)?;
        let first = first_sent(&results)?;
        let mut all_warnings = report["warnings"].as_array().cloned().unwrap_or_default();
        all_warnings.extend(warnings.into_iter().map(Value::String));
        Ok(
            json!({"post":first["post"],"message":first["message"],"results":results,"warnings":all_warnings}),
        )
    }

    pub fn dry_run_report(
        &self,
        chats: &[Value],
        post: &Value,
        warnings: Value,
    ) -> Result<Value, ServiceError> {
        let kind = kinds::get(Some(kinds::RICH))?;
        let mut requests = Vec::new();
        for chat in chats {
            let (method, params) = Publisher::request(chat, post, kind)?;
            collect_uploads(&params, &self.context.media_dir, &mut Uploads::default())
                .map_err(TelegramError::from)?;
            requests.push(json!({"method":method,"params":params,"chat_title":crate::services::channels::title_of(&self.context,chat)?}));
        }
        let mut steps = Vec::new();
        if post["pin"].as_bool() == Some(true) {
            steps.push(json!("pinChatMessage"));
        }
        if !post["comment"].is_null() {
            steps.push(json!("first comment"));
        }
        if !post["poll_after"].is_null() {
            steps.push(json!("sendPoll (poll_after)"));
        }
        Ok(json!({"dry_run":true,"requests":requests,"after_send":steps,"warnings":warnings}))
    }

    pub async fn preview(&self, post: &Value, chat_id: &Value) -> Result<Value, ServiceError> {
        let local_post = post.clone();
        let local_chat = chat_id.clone();
        let chat = self
            .run(move |service| {
                service.validate(&local_post)?;
                let target = if local_chat.is_null() || local_chat.as_str() == Some("") {
                    Value::String(
                        service
                            .context
                            .account_id
                            .map(|uid| uid.to_string())
                            .unwrap_or_else(|| service.settings.preview_chat.clone()),
                    )
                } else {
                    local_chat
                };
                if target.as_str() == Some("") || target.is_null() {
                    return Err(ServiceError::BadRequest(
                        "не задан TG_PREVIEW_CHAT (ваш user id) — некуда отправить предпросмотр"
                            .into(),
                    ));
                }
                service.resolve_chat(&target)
            })
            .await?;
        let result = self
            .publisher()
            .send(&chat, post, kinds::get(Some(kinds::RICH))?)
            .await?;
        let mut output = json!({"message_id":result["message_id"],"chat_id":chat});
        if let Some(blocks) = result["rich_message"]["blocks"].as_array() {
            output["parsed"] = json!({"blocks":blocks,"input_blocks":crate::spec::convert::to_input_blocks(&self.spec,blocks)});
        }
        if post["callbacks"]
            .as_object()
            .is_some_and(|callbacks| !callbacks.is_empty())
        {
            crate::services::updates::remember_preview(
                &self.context,
                &chat,
                result["message_id"].as_i64().unwrap_or_default(),
                &post["callbacks"],
            )
            .await?;
        }
        if !post["comment"].is_null() {
            let params = json!({"chat_id":chat,"rich_message":post["comment"],"reply_parameters":{"message_id":result["message_id"]}});
            let (params, uploads) = self
                .run(move |service| {
                    let mut uploads = Uploads::default();
                    let params = collect_uploads(&params, &service.context.media_dir, &mut uploads)
                        .map_err(TelegramError::from)?;
                    Ok((params, uploads))
                })
                .await?;
            self.transport
                .call(&self.context, "sendRichMessage", params, uploads)
                .await?;
        }
        Ok(output)
    }

    #[allow(clippy::too_many_arguments)] // Mirrors the shared REST/MCP schedule contract.
    pub fn schedule(
        &self,
        post: &Value,
        at: &Value,
        chat_id: &Value,
        chat_ids: &Value,
        draft_id: Option<&str>,
        actor: &str,
        dry_run: bool,
        as_new: bool,
    ) -> Result<Value, ServiceError> {
        self.not_an_edit_copy(draft_id, as_new)?;
        let report = self.validate(post)?;
        let when = parse_at(at)?;
        let chats = self.targets(chat_id, chat_ids, draft_id)?;
        self.check_thread(&chats, post)?;
        if dry_run {
            let mut report = self.dry_run_report(&chats, post, report["warnings"].clone())?;
            report["at"] = json!(when);
            return Ok(report);
        }
        let mut records = posts::add_scheduled(
            &self.context,
            &chats,
            post,
            when,
            draft_id,
            kinds::RICH,
            actor,
        )?;
        for record in &mut records {
            record["chat_title"] =
                crate::services::channels::title_of(&self.context, &record["chat_id"])?;
        }
        let mut first = records
            .first()
            .cloned()
            .ok_or_else(|| ServiceError::BadRequest("не указан канал".to_owned()))?;
        if records.len() > 1 {
            first["all"] = json!(records);
        }
        Ok(first)
    }

    #[allow(clippy::too_many_arguments)] // Same destination and schedule options as rich posts.
    pub async fn send_kind(
        &self,
        kind: &Kind,
        params: &Value,
        chat_id: &Value,
        chat_ids: &Value,
        at: &Value,
        pin: bool,
        actor: &str,
    ) -> Result<Value, ServiceError> {
        let prepared = kinds::prepare(kind, params)?;
        let mut post = serde_json::Map::new();
        post.insert(kind.key.unwrap_or_default().to_owned(), prepared);
        post.insert("pin".to_owned(), json!(pin));
        let post = Value::Object(post);
        let local_chat = chat_id.clone();
        let local_chats = chat_ids.clone();
        let chats = self
            .run(move |service| service.targets(&local_chat, &local_chats, None))
            .await?;
        if kind.name == kinds::WIDGET {
            reject_channels(&self.context, &chats)?;
        }
        if !at.is_null() {
            let when = parse_at(at)?;
            let chats = chats.clone();
            let post = post.clone();
            let kind = *kind;
            let actor = actor.to_owned();
            let records = self
                .run(move |service| {
                    Ok(posts::add_scheduled(
                        &service.context,
                        &chats,
                        &post,
                        when,
                        None,
                        kind.name,
                        &actor,
                    )?)
                })
                .await?;
            return Ok(json!({"scheduled":records}));
        }
        let (mut results, warnings) = self
            .publisher()
            .publish(&chats, &post, None, kind, actor)
            .await?;
        titled_results(&self.context, &mut results)?;
        let _ = first_sent(&results)?;
        Ok(json!({"results":results,"warnings":warnings}))
    }
}

/// Каналы не входят в прямую отправку нового виджета (Bot API 10.4): только личный чат,
/// обычные группы и супергруппы. Пересылка в канал разрешена, но это делает Telegram.
/// Неизвестные чаты пропускаем — ограничение дублируется на сервере Telegram.
fn reject_channels(context: &AccountContext, chats: &[Value]) -> Result<(), ServiceError> {
    for chat in chats {
        if let Some(channel) = crate::store::channels::get(context, chat)?
            && channel["type"].as_str() == Some("channel")
        {
            return Err(ServiceError::BadRequest(format!(
                "{}: виджеты в канал напрямую не отправляются (ограничение Bot API 10.4), \
                 только личный чат или группа",
                channel["title"].as_str().unwrap_or_default()
            )));
        }
    }
    Ok(())
}

/// The reference `_first_sent` names the channel on every result before choosing the first one.
fn titled_results(context: &AccountContext, results: &mut [Value]) -> Result<(), ServiceError> {
    for result in results {
        result["chat_title"] = crate::services::channels::title_of(context, &result["chat_id"])?;
    }
    Ok(())
}

fn first_sent(results: &[Value]) -> Result<Value, ServiceError> {
    if let Some(first) = results
        .iter()
        .find(|result| result["ok"].as_bool() == Some(true))
    {
        return Ok(first.clone());
    }
    let errors = results
        .iter()
        .map(|result| {
            let chat = result["chat_id"]
                .as_str()
                .map_or_else(|| result["chat_id"].to_string(), str::to_owned);
            format!("{chat}: {}", result["error"].as_str().unwrap_or_default())
        })
        .collect::<Vec<_>>();
    if results.iter().any(|result| {
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains(MAY_HAVE_GONE)
    }) {
        Err(ServiceError::MayHaveGone(
            json!({"message":"ни один канал не подтвердил отправку, в часть могло уйти","errors":errors}),
        ))
    } else {
        Err(ServiceError::Upstream(
            json!({"message":"ни в один канал не отправлено","errors":errors}),
        ))
    }
}

pub fn parse_at(at: &Value) -> Result<i64, ServiceError> {
    let timestamp = match at {
        Value::Number(number) => number.as_i64().ok_or_else(|| ServiceError::BadRequest("время: unix или ISO 8601".to_owned()))?,
        Value::String(text) if text.chars().all(|character| character.is_ascii_digit()) => text.parse::<i64>().map_err(|error| ServiceError::BadRequest(error.to_string()))?,
        Value::String(text) => OffsetDateTime::parse(text, &Iso8601::DEFAULT)
            .map_err(|_| ServiceError::BadRequest("время: unix или ISO 8601, например 2026-10-10T09:00:00+03:00; укажите часовой пояс".to_owned()))?.unix_timestamp(),
        _ => return Err(ServiceError::BadRequest("время: unix или ISO 8601".to_owned())),
    };
    if timestamp < now() - 60 {
        return Err(ServiceError::BadRequest(
            "время публикации уже прошло".to_owned(),
        ));
    }
    Ok(timestamp)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// The original API's Post.model_dump defaults are part of persisted request fingerprints.
pub fn normalize_post(post: &Value) -> Value {
    let Some(mut post) = post.as_object().cloned() else {
        return post.clone();
    };
    post.retain(|name, _| {
        [
            "rich_message",
            "reply_markup",
            "options",
            "pin",
            "comment",
            "callbacks",
            "poll_after",
        ]
        .contains(&name.as_str())
    });
    for (key, value) in [
        ("reply_markup", Value::Null),
        ("options", json!({})),
        ("pin", json!(false)),
        ("comment", Value::Null),
        ("callbacks", json!({})),
        ("poll_after", Value::Null),
    ] {
        post.entry(key.to_owned()).or_insert(value);
    }
    Value::Object(post)
}
