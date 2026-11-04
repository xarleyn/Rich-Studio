use crate::{
    account_context::AccountContext,
    errors::ServiceError,
    services::{blocking, posts::parse_at},
    store::suggested_posts as store,
    telegram::{TelegramError, client::TelegramTransport, uploads::Uploads},
};
use serde_json::{Value, json};
const DECIDABLE: &[&str] = &["pending", "failed"];
const MEDIA_KEYS: &[&str] = &[
    "photo",
    "video",
    "animation",
    "audio",
    "document",
    "voice",
    "video_note",
    "live_photo",
    "paid_media",
];
pub fn view(mut record: Value) -> Value {
    record["text"] = record["message"]["text"]
        .as_str()
        .or_else(|| record["message"]["caption"].as_str())
        .unwrap_or("")
        .into();
    record["media"] = json!(
        MEDIA_KEYS
            .iter()
            .filter(|key| record["message"]
                .get(**key)
                .is_some_and(|v| !v.is_null() && v != false))
            .collect::<Vec<_>>()
    );
    record
}
fn check(record: &Value) -> Result<(), ServiceError> {
    let state = record["state"].as_str().unwrap_or("");
    if ["approving", "declining"].contains(&state) {
        return Err(ServiceError::BadRequest("решение уже отправлено в Telegram. Проверьте итог в личных сообщениях канала перед повтором".to_owned()));
    }
    if !DECIDABLE.contains(&state) {
        return Err(ServiceError::BadRequest(format!(
            "предложенный пост уже {state}: решение принято"
        )));
    }
    Ok(())
}
#[derive(Clone)]
pub struct SuggestedPosts {
    context: AccountContext,
    transport: TelegramTransport,
}
impl SuggestedPosts {
    pub fn new(context: AccountContext, transport: TelegramTransport) -> Self {
        Self { context, transport }
    }
    async fn get(&self, id: &str) -> Result<Value, ServiceError> {
        let id = id.to_owned();
        blocking::run(&self.context,move |c|store::get(c,&id)?.ok_or_else(||ServiceError::NotFound(format!("предложенного поста {id} нет: студия видит только те, что пришли, пока она запущена")))).await
    }
    pub async fn listing(&self, state: Option<String>) -> Result<Value, ServiceError> {
        blocking::run(&self.context, move |c| {
            Ok(json!(
                store::listing(c, state.as_deref())?
                    .into_iter()
                    .map(view)
                    .collect::<Vec<_>>()
            ))
        })
        .await
    }
    async fn update(&self, id: &str, fields: Value) -> Result<Value, ServiceError> {
        let id = id.to_owned();
        blocking::run(&self.context, move |c| {
            Ok(view(store::update(c, &id, &fields)?.ok_or_else(|| {
                ServiceError::NotFound("Not Found".to_owned())
            })?))
        })
        .await
    }
    async fn decide(
        &self,
        record: &Value,
        method: &str,
        params: Value,
        deciding: &str,
    ) -> Result<(), ServiceError> {
        let id = record["id"].as_str().unwrap_or("").to_owned();
        let target = deciding.to_owned();
        if !blocking::run(&self.context, move |c| {
            Ok(store::claim(c, &id, DECIDABLE, &target)?)
        })
        .await?
        {
            return Err(ServiceError::Conflict(
                "предложенный пост решается в другом запросе".to_owned(),
            ));
        }
        match self
            .transport
            .call(&self.context, method, params, Uploads::default())
            .await
        {
            Ok(_) => Ok(()),
            Err(TelegramError::NoAnswer(detail)) => Err(ServiceError::MayHaveGone(json!(format!(
                "{detail}. Решение могло дойти до Telegram: проверьте личные сообщения канала"
            )))),
            Err(error) => {
                let id = record["id"].as_str().unwrap_or("").to_owned();
                let old = record["state"].as_str().unwrap_or("").to_owned();
                let deciding = deciding.to_owned();
                blocking::run(&self.context, move |c| {
                    Ok(store::claim(c, &id, &[&deciding], &old)?)
                })
                .await?;
                Err(ServiceError::Telegram(error))
            }
        }
    }
    pub async fn approve(
        &self,
        id: &str,
        send_date: &Value,
        actor: &str,
    ) -> Result<Value, ServiceError> {
        let record = self.get(id).await?;
        check(&record)?;
        let mut params = json!({"chat_id":record["chat_num"],"message_id":record["message_id"]});
        if !send_date.is_null() && *send_date != "" {
            if !record["send_date"].is_null() && record["send_date"] != 0 {
                return Err(ServiceError::BadRequest(
                    "время публикации уже выбрал автор предложения".to_owned(),
                ));
            }
            let at = parse_at(send_date)?;
            if at > crate::store::auth::now() + 2678400 {
                return Err(ServiceError::BadRequest(
                    "опубликовать предложение можно не позже чем через 30 дней".to_owned(),
                ));
            }
            params["send_date"] = json!(at);
        }
        self.decide(&record, "approveSuggestedPost", params.clone(), "approving")
            .await?;
        self.update(id,json!({"state":"approved","send_date":params.get("send_date").unwrap_or(&record["send_date"]),"actor":actor,"decided_at":crate::store::auth::now()})).await
    }
    pub async fn decline(
        &self,
        id: &str,
        comment: &Value,
        actor: &str,
    ) -> Result<Value, ServiceError> {
        let record = self.get(id).await?;
        check(&record)?;
        let comment = comment.as_str().unwrap_or("").trim();
        if comment.chars().count() > 128 {
            return Err(ServiceError::BadRequest(
                "комментарий длиннее 128 символов".to_owned(),
            ));
        }
        let mut params = json!({"chat_id":record["chat_num"],"message_id":record["message_id"]});
        if !comment.is_empty() {
            params["comment"] = json!(comment);
        }
        self.decide(&record, "declineSuggestedPost", params, "declining")
            .await?;
        self.update(id,json!({"state":"declined","comment":if comment.is_empty(){None}else{Some(comment)},"actor":actor,"decided_at":crate::store::auth::now()})).await
    }
    pub async fn reopen(&self, id: &str, actor: &str) -> Result<Value, ServiceError> {
        let record = self.get(id).await?;
        let state = record["state"].as_str().unwrap_or("").to_owned();
        if !["approving", "declining"].contains(&state.as_str()) {
            return Err(ServiceError::BadRequest(format!(
                "предложенный пост в состоянии {state}: решить снова можно только решение без ответа"
            )));
        }
        let key = id.to_owned();
        if !blocking::run(&self.context, move |c| {
            Ok(store::claim(c, &key, &[&state], "pending")?)
        })
        .await?
        {
            return Err(ServiceError::Conflict("итог пришёл от Telegram".to_owned()));
        }
        self.update(id, json!({"actor":actor})).await
    }
}
pub async fn on_message(context: &AccountContext, message: &Value) -> Result<bool, ServiceError> {
    let Some(info) = message["suggested_post_info"].as_object() else {
        return Ok(false);
    };
    let (Some(chat), Some(id)) = (
        message["chat"]["id"].as_i64(),
        message["message_id"].as_i64(),
    ) else {
        return Ok(false);
    };
    let state = info
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("pending");
    let fields = json!({"state":state,"send_date":message["suggested_post_info"]["send_date"],"price_amount":message["suggested_post_info"]["price"]["amount"],"price_currency":message["suggested_post_info"]["price"]["currency"]});
    let message = message.clone();
    blocking::run(context, move |c| {
        store::save(c, chat, id, &message, &fields)?;
        Ok(true)
    })
    .await
}
pub async fn on_service(context: &AccountContext, message: &Value) -> Result<bool, ServiceError> {
    for (key, state) in [
        ("suggested_post_approved", "approved"),
        ("suggested_post_declined", "declined"),
        ("suggested_post_paid", "paid"),
        ("suggested_post_refunded", "refunded"),
        ("suggested_post_approval_failed", "failed"),
    ] {
        if !message[key].is_object() {
            continue;
        }
        let original = message[key]
            .get("suggested_post_message")
            .or_else(|| message.get("reply_to_message"))
            .cloned()
            .unwrap_or(Value::Null);
        let (Some(chat), Some(id)) = (
            message["chat"]["id"].as_i64(),
            original["message_id"].as_i64(),
        ) else {
            return Ok(false);
        };
        let mut fields = json!({"state":state});
        if key == "suggested_post_approved" && !message[key]["send_date"].is_null() {
            fields["send_date"] = message[key]["send_date"].clone();
        }
        if key == "suggested_post_declined" && !message[key]["comment"].is_null() {
            fields["comment"] = message[key]["comment"].clone();
        }
        return blocking::run(context, move |c| {
            let key = store::key(chat, id);
            if store::get(c, &key)?.is_some() {
                store::update(c, &key, &fields)?;
            } else if original["chat"].is_object() {
                store::save(c, chat, id, &original, &fields)?;
            } else {
                return Ok(false);
            }
            Ok(true)
        })
        .await;
    }
    Ok(false)
}
