//! Durable personal reports. Notification failure never changes publication success.
use super::{accounts, blocking, bot_dialog, listing};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    settings::Settings,
    store::{auth, bot_dialog as db, channels, posts},
    telegram::{TelegramError, client::TelegramTransport},
};
use serde_json::{Value, json};

pub async fn record(
    context: &AccountContext,
    settings: &Settings,
    post: &Value,
    detail: Option<&str>,
) -> Result<(), ServiceError> {
    record_event(
        context,
        settings,
        post,
        post["status"].as_str().unwrap_or("failed"),
        detail,
    )
    .await
}
pub async fn record_event(
    context: &AccountContext,
    settings: &Settings,
    post: &Value,
    event: &str,
    detail: Option<&str>,
) -> Result<(), ServiceError> {
    let settings = settings.clone();
    let post = post.clone();
    let event = event.to_owned();
    let detail = detail.map(str::to_owned);
    blocking::run(context,move |context|{
        let Some(uid)=bot_dialog::recipient(context,&settings)else{return Ok(())};
        let Some(prefs)=db::contact(&settings,uid)?else{return Ok(())};
        if prefs["mode"]=="off"{return Ok(())}
        let detail=detail.as_deref().filter(|s|!s.is_empty()).unwrap_or_else(||post["error"].as_str().unwrap_or(""));
        let category=if event=="sent"&&detail.is_empty(){"success"}else{"error"};
        if prefs["mode"]=="errors"&&category=="success"{return Ok(())}
        let chat=channels::get(context,&post["chat_id"])?;
        let chat_name=chat.as_ref().and_then(|chat|chat["title"].as_str()).or_else(||post["chat_id"].as_str()).map(str::to_owned).unwrap_or_else(||post["chat_id"].to_string());
        let body=crate::logs::scrub_all(&settings,&json!({"event":event,"chat":chat_name.chars().take(160).collect::<String>(),"title":listing::title(&post["payload"],post["kind"].as_str())?,"detail":detail.chars().take(700).collect::<String>(),"link":listing::link(&post["chat_id"],&post["message_id"]),"post_id":post["id"],"subscription":prefs["revision"],"scheduled_at":post["scheduled_at"]}));
        let mut key=format!("{}:{event}",post["id"].as_str().unwrap_or(""));
        if ["missed","retry"].contains(&event.as_str()){key.push_str(&format!(":{}",post["scheduled_at"]));}
        db::enqueue(context,&key,uid,category,&body)?;Ok(())
    }).await
}
pub async fn expire_comments(
    context: &AccountContext,
    settings: &Settings,
    before: i64,
    error: &str,
) -> Result<(), ServiceError> {
    let error = error.to_owned();
    let ids = blocking::run(context, move |context| {
        Ok(db::expire_comments(context, before, &error)?)
    })
    .await?;
    for id in ids {
        if let Some(post) =
            blocking::run(context, move |context| Ok(posts::get(context, &id)?)).await?
        {
            record_event(context, settings, &post, "comment", None).await?;
        }
    }
    Ok(())
}
pub fn text(body: &Value, language: &str) -> String {
    let outcome = body["event"].as_str().unwrap_or("failed");
    let detail = body["detail"].as_str().unwrap_or("");
    let (ru, en) = match outcome {
        "sent" if !detail.is_empty() => (
            "⚠️ Пост опубликован с предупреждением",
            "⚠️ Post published with a warning",
        ),
        "sent" => ("✅ Пост опубликован", "✅ Post published"),
        "uncertain" => ("⚠️ Пост мог уйти в канал", "⚠️ The post may have been sent"),
        "missed" => ("⏰ Публикация пропущена", "⏰ Scheduled post missed"),
        "retry" => ("⏳ Публикация задерживается", "⏳ Publication delayed"),
        "comment" => (
            "⚠️ Не удалось отправить первый комментарий",
            "⚠️ First comment failed",
        ),
        _ => (
            "❌ Не удалось опубликовать пост",
            "❌ Could not publish the post",
        ),
    };
    let mut text = format!(
        "<b>{}</b>\n{}",
        bot_dialog::phrase(language, ru, en),
        html_escape::encode_text(body["chat"].as_str().unwrap_or(""))
    );
    for value in [body["title"].as_str().unwrap_or(""), detail] {
        if !value.is_empty() {
            text.push_str(&format!("\n\n{}", html_escape::encode_text(value)));
        }
    }
    if outcome == "uncertain" {
        text.push_str(bot_dialog::phrase(language,"\n\nПроверьте канал перед повтором. Автоматически повторять эту отправку не буду.","\n\nCheck the channel before retrying. This publication will not be retried automatically."));
    } else if ["failed", "missed", "comment"].contains(&outcome) {
        text.push_str(bot_dialog::phrase(
            language,
            "\n\nОткройте «Посты» в студии, чтобы проверить подробности.",
            "\n\nOpen Posts in the studio to check the details.",
        ));
    }
    text.push_str(bot_dialog::phrase(
        language,
        "\n\nНастройки: /notifications",
        "\n\nSettings: /notifications",
    ));
    text
}
pub async fn deliver_one(
    context: &AccountContext,
    settings: &Settings,
    transport: &TelegramTransport,
    event: &Value,
) -> Result<(), ServiceError> {
    let event = event.clone();
    let settings_copy = settings.clone();
    let claimed = blocking::run(context, move |context| {
        let uid = event["recipient"].as_i64().unwrap_or(0);
        let key = event["event_key"].as_str().unwrap_or("");
        let prefs = db::contact(&settings_copy, uid)?;
        let allowed = prefs.as_ref().is_some_and(|prefs| {
            bot_dialog::recipient(context, &settings_copy) == Some(uid)
                && prefs["mode"] != "off"
                && event["body"]["subscription"] == prefs["revision"]
                && !(prefs["mode"] == "errors" && event["category"] == "success")
        });
        if !allowed {
            db::finish(context, key, "skipped", 0)?;
            return Ok(None);
        }
        let outcome = event["body"]["event"].as_str().unwrap_or("");
        if ["retry", "missed"].contains(&outcome) {
            let post = posts::get(context, event["body"]["post_id"].as_str().unwrap_or(""))?
                .unwrap_or(Value::Null);
            let expected = if outcome == "retry" {
                "scheduled"
            } else {
                "missed"
            };
            if post["status"] != expected
                || post["scheduled_at"] != event["body"]["scheduled_at"]
                || (outcome == "retry" && post["retry_at"].is_null())
            {
                db::finish(context, key, "skipped", 0)?;
                return Ok(None);
            }
        }
        if !db::claim(context, key)? {
            return Ok(None);
        }
        Ok(prefs.map(|prefs| (event, prefs)))
    })
    .await?;
    let Some((event, prefs)) = claimed else {
        return Ok(());
    };
    let uid = event["recipient"].as_i64().unwrap_or(0);
    let language = prefs["language"].as_str().unwrap_or("ru");
    let result = bot_dialog::send_private(
        context,
        settings,
        transport,
        uid,
        &text(&event["body"], language),
        bot_dialog::keyboard(settings, language, false, event["body"]["link"].as_str()),
    )
    .await;
    let (state, retry_at, disable) = match result {
        Ok(()) => ("sent", 0, false),
        Err(ServiceError::Telegram(TelegramError::NoAnswer(_))) => {
            tracing::warn!(
                event = "bot.delivery_uncertain",
                "notification may have been delivered; not retrying"
            );
            return Ok(()); // Keep attempted: it may exist.
        }
        Err(ServiceError::Telegram(error)) => {
            let disable = matches!(
                error,
                TelegramError::Refused {
                    code: Some(403),
                    ..
                }
            );
            let retry = match error {
                TelegramError::Unreachable(_) => Some(60),
                TelegramError::RateLimited {
                    retry_after: Some(wait),
                    ..
                } if wait > 0 => Some(wait.max(60)),
                _ => None,
            };
            if event["attempts"].as_i64().unwrap_or(0) < 4 && retry.is_some() {
                (
                    "pending",
                    auth::now().saturating_add(retry.unwrap_or(60).min(i64::MAX as u64) as i64),
                    disable,
                )
            } else {
                ("failed", 0, disable)
            }
        }
        Err(_) => return Ok(()),
    };
    let settings = settings.clone();
    let key = event["event_key"].as_str().unwrap_or("").to_owned();
    blocking::run(context, move |context| {
        if disable {
            db::disable(&settings, uid)?;
        }
        db::finish(context, &key, state, retry_at)?;
        Ok(())
    })
    .await
}
pub async fn deliver(state: &AppState) -> Result<(), ServiceError> {
    let settings = state.settings.clone();
    let contexts = blocking::run(&AccountContext::root(&settings.clone()), move |_| {
        accounts::all_contexts(&settings).map_err(|error| ServiceError::Internal(error.to_string()))
    })
    .await?;
    let transport = state.transport();
    let mut errors = Vec::new();
    for context in contexts {
        let events = blocking::run(&context, |context| Ok(db::due(context, 10)?)).await;
        if let Ok(events) = events {
            for event in events {
                if let Err(error) = deliver_one(&context, &state.settings, &transport, &event).await
                {
                    errors.push(error.to_string());
                }
            }
        } else if let Err(error) = events {
            errors.push(error.to_string());
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ServiceError::Internal(errors.join("; ")))
    }
}
