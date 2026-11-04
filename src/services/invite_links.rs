use crate::{
    account_context::AccountContext,
    errors::ServiceError,
    services::{blocking, posts::PostService},
    store::invite_links as store,
    telegram::{client::TelegramTransport, uploads::Uploads},
};
use serde_json::{Value, json};
pub const SUBSCRIPTION_PERIOD: i64 = 2592000;
#[derive(Clone)]
pub struct InviteLinks {
    context: AccountContext,
    posts: PostService,
    transport: TelegramTransport,
}
impl InviteLinks {
    pub fn new(context: AccountContext, posts: PostService, transport: TelegramTransport) -> Self {
        Self {
            context,
            posts,
            transport,
        }
    }
    async fn chat(&self, value: &Value) -> Result<Value, ServiceError> {
        let posts = self.posts.clone();
        let value = value.clone();
        blocking::run(&self.context, move |_| posts.resolve_chat(&value)).await
    }
    async fn get(&self, link: &str) -> Result<Value, ServiceError> {
        let link = link.trim().to_owned();
        blocking::run(&self.context, move |context| {
            store::get(context, &link)?.ok_or_else(|| {
                ServiceError::NotFound(format!("ссылки {link} нет среди созданных студией"))
            })
        })
        .await
    }
    pub async fn listing(&self, chat: &Value) -> Result<Value, ServiceError> {
        let chat = if absent(chat) {
            None
        } else {
            Some(text(&self.chat(chat).await?))
        };
        blocking::run(&self.context, move |context| {
            Ok(json!(store::listing(context, chat.as_deref())?))
        })
        .await
    }
    async fn save(
        &self,
        record: Value,
        chat: Value,
        link: Option<String>,
        actor: &str,
    ) -> Result<Value, ServiceError> {
        let link = link
            .or_else(|| record["invite_link"].as_str().map(str::to_owned))
            .ok_or_else(|| ServiceError::MayHaveGone(json!("Telegram не вернул ссылку")))?;
        let chat = text(&chat);
        let actor = actor.to_owned();
        blocking::run(&self.context, move |context| {
            Ok(store::save(context, &link, &chat, &actor, &record)?)
        })
        .await
    }
    pub async fn create(&self, body: &Value, actor: &str) -> Result<Value, ServiceError> {
        let chat = self.chat(&body["chat_id"]).await?;
        let mut params = json!({"chat_id":chat});
        if let Some(name) = name(&body["name"])? {
            params["name"] = json!(name);
        }
        let price = &body["subscription_price"];
        let method = if !price.is_null() && *price != 0 {
            let price = price
                .as_i64()
                .filter(|v| (1..=10000).contains(v))
                .ok_or_else(|| {
                    ServiceError::BadRequest(
                        "subscription_price: от 1 до 10000 звёзд в месяц".to_owned(),
                    )
                })?;
            if !absent(&body["expire_date"])
                || !absent(&body["member_limit"])
                || body["creates_join_request"] == true
            {
                return Err(ServiceError::BadRequest("у платной подписки нет срока, предела участников и заявок: только название и цена".to_owned()));
            }
            params["subscription_period"] = json!(SUBSCRIPTION_PERIOD);
            params["subscription_price"] = json!(price);
            "createChatSubscriptionInviteLink"
        } else {
            limits(
                &mut params,
                expire(&body["expire_date"])?,
                &body["member_limit"],
                body["creates_join_request"] == true,
            )?;
            "createChatInviteLink"
        };
        let result = self
            .transport
            .call(&self.context, method, params, Uploads::default())
            .await?;
        self.save(result, chat, None, actor).await
    }
    pub async fn edit(&self, body: &Value) -> Result<Value, ServiceError> {
        let record = self.get(body["invite_link"].as_str().unwrap_or("")).await?;
        if record["is_revoked"] == 1 {
            return Err(ServiceError::BadRequest(
                "ссылка отозвана: её уже не изменить".to_owned(),
            ));
        }
        let chat = self.chat(&record["chat_id"]).await?;
        let kept = if body["name"].is_null() {
            record["name"].as_str().map(str::to_owned)
        } else {
            name(&body["name"])?
        };
        let mut params = json!({"chat_id":chat,"invite_link":record["invite_link"],"name":kept.unwrap_or_default()});
        let method = if record["kind"] == "subscription" {
            if !absent(&body["expire_date"])
                || !absent(&body["member_limit"])
                || body["creates_join_request"] == true
            {
                return Err(ServiceError::BadRequest(
                    "у платной подписки меняется только название".to_owned(),
                ));
            }
            "editChatSubscriptionInviteLink"
        } else {
            let expiry = if body["expire_date"].is_null() {
                record["expire_date"].as_i64()
            } else {
                expire(&body["expire_date"])?
            };
            let limit = if body["member_limit"].is_null() {
                &record["member_limit"]
            } else {
                &body["member_limit"]
            };
            let requests = if body["creates_join_request"].is_null() {
                record["creates_join_request"] == 1
            } else {
                body["creates_join_request"] == true
            };
            limits(&mut params, expiry, limit, requests)?;
            "editChatInviteLink"
        };
        let result = self
            .transport
            .call(&self.context, method, params, Uploads::default())
            .await?;
        self.save(
            result,
            record["chat_id"].clone(),
            record["invite_link"].as_str().map(str::to_owned),
            "",
        )
        .await
    }
    pub async fn revoke(&self, link: &str) -> Result<Value, ServiceError> {
        let record = self.get(link).await?;
        if record["is_revoked"] == 1 {
            return Ok(record);
        }
        let chat = self.chat(&record["chat_id"]).await?;
        let mut result = self
            .transport
            .call(
                &self.context,
                "revokeChatInviteLink",
                json!({"chat_id":chat,"invite_link":record["invite_link"]}),
                Uploads::default(),
            )
            .await?;
        result["is_revoked"] = json!(true);
        self.save(
            result,
            record["chat_id"].clone(),
            record["invite_link"].as_str().map(str::to_owned),
            "",
        )
        .await
    }
}
fn text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}
fn absent(value: &Value) -> bool {
    value.is_null() || value == "" || value == 0
}
fn name(value: &Value) -> Result<Option<String>, ServiceError> {
    let value = value.as_str().unwrap_or("").trim();
    if value.chars().count() > 32 {
        return Err(ServiceError::BadRequest(
            "название ссылки длиннее 32 символов: столько Telegram не примет".to_owned(),
        ));
    }
    Ok((!value.is_empty()).then(|| value.to_owned()))
}
fn expire(value: &Value) -> Result<Option<i64>, ServiceError> {
    if absent(value) {
        Ok(None)
    } else {
        super::posts::parse_at(value).map(Some)
    }
}
fn limits(
    params: &mut Value,
    expiry: Option<i64>,
    limit: &Value,
    requests: bool,
) -> Result<(), ServiceError> {
    if let Some(expiry) = expiry {
        params["expire_date"] = json!(expiry);
    }
    if !limit.is_null() && *limit != 0 {
        let limit = limit
            .as_i64()
            .filter(|v| (1..=99999).contains(v))
            .ok_or_else(|| ServiceError::BadRequest("member_limit: от 1 до 99999".to_owned()))?;
        if requests {
            return Err(ServiceError::BadRequest(
                "у ссылки с заявками на вступление не бывает предела участников: оставьте одно"
                    .to_owned(),
            ));
        }
        params["member_limit"] = json!(limit);
    }
    if requests {
        params["creates_join_request"] = json!(true);
    }
    Ok(())
}
fn inside(member: &Value) -> bool {
    matches!(
        member["status"].as_str(),
        Some("member" | "administrator" | "creator")
    ) || member["status"] == "restricted" && member["is_member"] == true
}
pub async fn on_member(context: &AccountContext, event: &Value) -> Result<bool, ServiceError> {
    let Some(link) = event["invite_link"]["invite_link"].as_str() else {
        return Ok(false);
    };
    if inside(&event["old_chat_member"]) || !inside(&event["new_chat_member"]) {
        return Ok(false);
    }
    let link = link.to_owned();
    blocking::run(context, move |context| {
        Ok(store::count(context, &link, true)?)
    })
    .await
}
pub async fn on_join_request(
    context: &AccountContext,
    event: &Value,
) -> Result<bool, ServiceError> {
    let Some(link) = event["invite_link"]["invite_link"].as_str() else {
        return Ok(false);
    };
    let link = link.to_owned();
    blocking::run(context, move |context| {
        Ok(store::count(context, &link, false)?)
    })
    .await
}
