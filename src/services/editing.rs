use serde_json::{Value, json};

use crate::account_context::AccountContext;
use crate::errors::ServiceError;
use crate::services::posts::{PostService, parse_at};
use crate::services::{blocking, kinds};
use crate::settings::Settings;
use crate::spec::source::Spec;
use crate::store::{editing, posts};
use crate::telegram::TelegramError;
use crate::telegram::client::TelegramTransport;
use crate::telegram::uploads::{Uploads, collect_uploads};

const WAITING: &[&str] = &["scheduled", "missed"];
const NOT_MODIFIED: &str = "message is not modified";

#[derive(Clone)]
pub struct EditingService {
    context: AccountContext,
    transport: TelegramTransport,
    posts: PostService,
}

fn missing() -> ServiceError {
    ServiceError::NotFound("Not Found".to_owned())
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64() != Some(0.0),
    }
}

pub fn after_edit(old: &Value, post: &Value) -> Value {
    let mut result = old.as_object().cloned().unwrap_or_default();
    result.insert("rich_message".to_owned(), post["rich_message"].clone());
    if truthy(&post["reply_markup"]) {
        result.insert("reply_markup".to_owned(), post["reply_markup"].clone());
    } else {
        result.remove("reply_markup");
    }
    for key in ["callbacks", "comment"] {
        if truthy(&post[key]) {
            result.insert(key.to_owned(), post[key].clone());
        }
    }
    Value::Object(result)
}

impl EditingService {
    pub fn new(
        context: AccountContext,
        transport: TelegramTransport,
        settings: &Settings,
        spec: Spec,
    ) -> Self {
        Self {
            posts: PostService::new(context.clone(), transport.clone(), settings, spec),
            context,
            transport,
        }
    }

    pub async fn get(&self, id: &str) -> Result<Value, ServiceError> {
        let id = id.to_owned();
        blocking::run(&self.context, move |context| {
            posts::get(context, &id)?.ok_or_else(missing)
        })
        .await
    }

    async fn sent(&self, id: &str) -> Result<Value, ServiceError> {
        let record = self.get(id).await?;
        if record["status"] != "sent" || record["message_id"].as_i64().is_none_or(|id| id == 0) {
            return Err(ServiceError::BadRequest(format!(
                "пост в статусе {}: он ещё не в канале",
                record["status"]
            )));
        }
        Ok(record)
    }

    async fn resolve(&self, chat: &Value) -> Result<Value, ServiceError> {
        let service = self.posts.clone();
        let chat = chat.clone();
        blocking::run(&self.context, move |_| service.resolve_chat(&chat)).await
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, TelegramError> {
        let context = self.context.clone();
        let uploads = tokio::task::spawn_blocking(move || {
            let mut uploads = Uploads::default();
            let params = collect_uploads(&params, &context.media_dir, &mut uploads)?;
            Ok::<_, crate::telegram::uploads::UploadError>((params, uploads))
        })
        .await
        .map_err(|_| TelegramError::Policy("операция загрузки прервана".to_owned()))??;
        self.transport
            .call(&self.context, method, uploads.0, uploads.1)
            .await
    }

    async fn claim(&self, id: &str, expected: Option<i64>) -> Result<i64, ServiceError> {
        let id = id.to_owned();
        blocking::run(&self.context,move |context| editing::claim_version(context,&id,expected)?.ok_or_else(|| ServiceError::Conflict(format!("пост {id} уже заменили: прочитайте его заново (get_post) и повторите правку поверх новой версии")))).await
    }

    async fn release(&self, id: &str, taken: i64) -> Result<(), ServiceError> {
        let id = id.to_owned();
        blocking::run(&self.context, move |context| {
            Ok(editing::release_version(context, &id, taken)?)
        })
        .await
    }

    async fn replace(
        &self,
        record: &Value,
        new: &Value,
        actor: &str,
        taken: i64,
        waiting: bool,
    ) -> Result<(), ServiceError> {
        let id = record["id"].as_str().ok_or_else(missing)?.to_owned();
        let old = record["payload"].clone();
        let new = new.clone();
        let actor = actor.to_owned();
        blocking::run(&self.context, move |context| {
            let changed = if waiting {
                editing::replace_scheduled(context, &id, &new, &old, &actor, taken)?
            } else {
                editing::replace_sent(context, &id, &new, &old, &actor, taken)?
            };
            if !changed {
                return Err(ServiceError::Conflict(
                    "пост уже отправляется или изменился: обновите список".to_owned(),
                ));
            }
            Ok(())
        })
        .await
    }

    pub async fn edit(
        &self,
        id: &str,
        post: &Value,
        actor: &str,
        expected: Option<i64>,
    ) -> Result<Value, ServiceError> {
        let record = self.get(id).await?;
        let status = record["status"].as_str().unwrap_or_default();
        if !["scheduled", "missed", "sent"].contains(&status) {
            return Err(ServiceError::BadRequest(format!(
                "пост в статусе {status}, заменить нельзя"
            )));
        }
        let kind = kinds::get(record["kind"].as_str())?;
        if !kind.editable {
            return Err(ServiceError::BadRequest(if kind.stoppable {
                "опрос заменить нельзя; можно закрыть или удалить".to_owned()
            } else {
                format!(
                    "{} заменить нельзя; можно удалить или сменить кнопки",
                    kind.label
                )
            }));
        }
        if status == "sent" && record["message_id"].as_i64().is_none_or(|id| id == 0) {
            return Err(ServiceError::BadRequest("редактировать нечего".to_owned()));
        }
        self.posts.validate(post)?;
        let taken = self
            .claim(id, expected.or_else(|| record["version"].as_i64()))
            .await?;
        if WAITING.contains(&status) {
            if let Err(error) = self.replace(&record, post, actor, taken, true).await {
                self.release(id, taken).await?;
                return Err(error);
            }
        } else {
            self.edit_record(&record, post, actor, taken).await?;
        }
        self.get(id).await
    }

    async fn edit_record(
        &self,
        record: &Value,
        post: &Value,
        actor: &str,
        taken: i64,
    ) -> Result<Value, ServiceError> {
        let id = record["id"].as_str().ok_or_else(missing)?;
        let chat = match self.resolve(&record["chat_id"]).await {
            Ok(chat) => chat,
            Err(error) => {
                self.release(id, taken).await?;
                return Err(error);
            }
        };
        let params = edit_params(chat, record["message_id"].clone(), post);
        let result = match self.call("editMessageText", params).await {
            Ok(result) => result,
            Err(TelegramError::Refused { description, .. })
                if description.contains(NOT_MODIFIED) =>
            {
                Value::Null
            }
            Err(TelegramError::NoAnswer(error)) => {
                return Err(uncertain(&error, "Правка могла пройти"));
            }
            Err(error) => {
                self.release(id, taken).await?;
                return Err(error.into());
            }
        };
        let new = after_edit(&record["payload"], post);
        if new != record["payload"] {
            self.replace(record, &new, actor, taken, false)
                .await
                .map_err(|error| uncertain(&error.to_string(), "Правка могла пройти"))?;
        }
        Ok(json!({"result":result,"unchanged":result.is_null()}))
    }

    pub async fn edit_message(
        &self,
        post: &Value,
        chat: &Value,
        message_id: i64,
        actor: &str,
    ) -> Result<Value, ServiceError> {
        self.posts.validate(post)?;
        let chat = self.resolve(chat).await?;
        let lookup_chat = chat.clone();
        let record = blocking::run(&self.context, move |context| {
            Ok(editing::find_sent(context, &lookup_chat, message_id)?)
        })
        .await?;
        if let Some(record) = record
            && kinds::get(record["kind"].as_str())?.editable
        {
            let id = record["id"].as_str().ok_or_else(missing)?;
            let taken = self.claim(id, record["version"].as_i64()).await?;
            let result = self.edit_record(&record, post, actor, taken).await?;
            // Raw edit historically creates a version only when the stored content changes.
            // Release only after a definite acknowledgement; an unknown outcome keeps its claim.
            if after_edit(&record["payload"], post) == record["payload"] {
                self.release(id, taken).await?;
            }
            return Ok(result);
        }
        let result = match self
            .call(
                "editMessageText",
                edit_params(chat, json!(message_id), post),
            )
            .await
        {
            Ok(result) => result,
            Err(TelegramError::Refused { description, .. })
                if description.contains(NOT_MODIFIED) =>
            {
                Value::Null
            }
            Err(TelegramError::NoAnswer(error)) => {
                return Err(uncertain(&error, "Правка могла пройти"));
            }
            Err(error) => return Err(error.into()),
        };
        Ok(json!({"result":result,"unchanged":result.is_null()}))
    }

    pub async fn versions(&self, id: &str) -> Result<Value, ServiceError> {
        self.get(id).await?;
        let id = id.to_owned();
        blocking::run(&self.context, move |context| {
            Ok(json!(editing::versions(context, &id)?))
        })
        .await
    }

    pub async fn reschedule(&self, id: &str, at: &Value) -> Result<Value, ServiceError> {
        let at = parse_at(at)?;
        let record = self.get(id).await?;
        let owned = id.to_owned();
        if !blocking::run(&self.context, move |context| {
            Ok(editing::reschedule(context, &owned, at)?)
        })
        .await?
        {
            return Err(ServiceError::BadRequest(format!(
                "пост в статусе {}: перенести можно отложенный или пропущенный",
                record["status"]
            )));
        }
        self.get(id).await
    }

    pub async fn delete(&self, id: &str) -> Result<Value, ServiceError> {
        let mut record = self.get(id).await?;
        let owned = id.to_owned();
        if record["status"] == "failed" {
            blocking::run(&self.context, move |context| {
                Ok(editing::remove(context, &owned)?)
            })
            .await?;
            record["status"] = json!("removed");
            return Ok(record);
        }
        if record["status"] == "sent" && truthy(&record["message_id"]) {
            self.call("deleteMessage",json!({"chat_id":self.resolve(&record["chat_id"]).await?,"message_id":record["message_id"]})).await?;
            blocking::run(&self.context, move |context| {
                Ok(editing::deleted(context, &owned)?)
            })
            .await?;
            return self.get(id).await;
        }
        if WAITING.contains(&record["status"].as_str().unwrap_or_default())
            && blocking::run(&self.context, move |context| {
                Ok(editing::cancel(context, &owned)?)
            })
            .await?
        {
            return self.get(id).await;
        }
        Err(ServiceError::BadRequest(format!(
            "пост в статусе {}: удалять или отменять нечего",
            record["status"]
        )))
    }

    pub async fn pin(
        &self,
        id: &str,
        pinned: bool,
        disable_notification: bool,
    ) -> Result<Value, ServiceError> {
        let record = self.sent(id).await?;
        let mut params = json!({"chat_id":self.resolve(&record["chat_id"]).await?,"message_id":record["message_id"]});
        if pinned {
            params["disable_notification"] = json!(disable_notification);
        }
        self.call(
            if pinned {
                "pinChatMessage"
            } else {
                "unpinChatMessage"
            },
            params,
        )
        .await?;
        let owned = id.to_owned();
        blocking::run(&self.context, move |context| {
            Ok(posts::set_pinned(context, &owned, pinned)?)
        })
        .await?;
        self.get(id).await
    }

    pub async fn buttons(
        &self,
        id: &str,
        markup: &Value,
        callbacks: &Value,
        actor: &str,
    ) -> Result<Value, ServiceError> {
        let bad = crate::spec::validator::keyboard_errors(markup);
        if !bad.is_empty() {
            return Err(ServiceError::Invalid(
                json!({"message":"кнопки не прошли проверку","ok":false,"errors":bad,"warnings":[]}),
            ));
        }
        let record = self.get(id).await?;
        let status = record["status"].as_str().unwrap_or_default();
        if !["sent", "scheduled", "missed"].contains(&status) {
            return Err(ServiceError::BadRequest(format!("пост в статусе {status}")));
        }
        let kind = kinds::get(record["kind"].as_str())?;
        let mut payload = record["payload"].clone();
        let holder = if let Some(key) = kind.key {
            &mut payload[key]
        } else {
            &mut payload
        };
        if !holder.is_object() {
            *holder = json!({});
        }
        if truthy(&markup["inline_keyboard"]) {
            holder["reply_markup"] = markup.clone();
        } else if let Some(object) = holder.as_object_mut() {
            object.remove("reply_markup");
        }
        let new_markup = holder["reply_markup"].clone();
        if !callbacks.is_null() {
            payload["callbacks"] = callbacks.clone();
        }
        if payload == record["payload"] {
            return Ok(record);
        }
        let taken = self.claim(id, record["version"].as_i64()).await?;
        if WAITING.contains(&status) {
            if let Err(error) = self.replace(&record, &payload, actor, taken, true).await {
                self.release(id, taken).await?;
                return Err(error);
            }
        } else {
            let chat = match self.resolve(&record["chat_id"]).await {
                Ok(chat) => chat,
                Err(error) => {
                    self.release(id, taken).await?;
                    return Err(error);
                }
            };
            let mut params = json!({"chat_id":chat,"message_id":record["message_id"]});
            if truthy(&new_markup) {
                params["reply_markup"] = new_markup;
            }
            match self.call("editMessageReplyMarkup", params).await {
                Ok(_) => {}
                Err(TelegramError::Refused { description, .. })
                    if description.contains(NOT_MODIFIED) => {}
                Err(TelegramError::NoAnswer(error)) => {
                    return Err(uncertain(&error, "Кнопки могли смениться"));
                }
                Err(error) => {
                    self.release(id, taken).await?;
                    return Err(error.into());
                }
            }
            self.replace(&record, &payload, actor, taken, false)
                .await
                .map_err(|error| uncertain(&error.to_string(), "Кнопки могли смениться"))?;
        }
        self.get(id).await
    }

    pub async fn stop_poll(&self, id: &str) -> Result<Value, ServiceError> {
        let record = self.sent(id).await?;
        if !kinds::get(record["kind"].as_str())?.stoppable {
            return Err(ServiceError::BadRequest("это не опрос".to_owned()));
        }
        let result = self.call("stopPoll",json!({"chat_id":self.resolve(&record["chat_id"]).await?,"message_id":record["message_id"]})).await?;
        let result = json!({"total":result["total_voter_count"].as_i64().unwrap_or(0),"is_closed":result["is_closed"].as_bool().unwrap_or(false),"options":result["options"].as_array().into_iter().flatten().map(|option| json!({"text":option["text"],"voter_count":option["voter_count"].as_i64().unwrap_or(0)})).collect::<Vec<_>>()});
        let owned = id.to_owned();
        blocking::run(&self.context, move |context| {
            Ok(editing::poll_results(context, &owned, &result)?)
        })
        .await?;
        self.get(id).await
    }
}

fn edit_params(chat: Value, message: Value, post: &Value) -> Value {
    let mut params =
        json!({"chat_id":chat,"message_id":message,"rich_message":post["rich_message"]});
    if truthy(&post["reply_markup"]) {
        params["reply_markup"] = post["reply_markup"].clone();
    }
    params
}

fn uncertain(error: &str, label: &str) -> ServiceError {
    ServiceError::MayHaveGone(json!(format!(
        "{error}. {label}: проверьте пост в канале (get_post покажет записанное)"
    )))
}
