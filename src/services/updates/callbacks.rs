use super::{
    preview::{has_callbacks, preview},
    routing::{find_message, transport},
};
use crate::{
    AppState,
    account_context::AccountContext,
    errors::ServiceError,
    services::{blocking, bot_dialog},
    store::{auth, bot_dialog as db},
    telegram::uploads::Uploads,
};
use serde_json::{Value, json};
use std::time::Duration;

async fn send_reply(
    state: &AppState,
    context: &AccountContext,
    id: &str,
    reply: Value,
) -> Result<(), String> {
    let media = context.media_dir.clone();
    // This worker only resolves account-local files. The Telegram future stays owned by the
    // caller, so cancelling the four-second budget cannot leave a detached sender.
    let (reply, uploads) = tokio::task::spawn_blocking(move || {
        let mut uploads = Uploads::default();
        let payload = crate::telegram::uploads::collect_uploads(&reply, &media, &mut uploads)?;
        Ok::<_, crate::telegram::TelegramError>((payload, uploads))
    })
    .await
    .map_err(|error| format!("не удалось прочитать файл rich-ответа: {error}"))?
    .map_err(|error| error.to_string())?;
    transport(state)
        .send_callback_reply(context, id, reply, uploads)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub(super) async fn answer(
    state: &AppState,
    context: &AccountContext,
    callback: &Value,
) -> Result<(), ServiceError> {
    let callback = callback.clone();
    let callback_copy = callback.clone();
    let lookup = blocking::run(context, move |context| {
        let message = &callback_copy["message"];
        let data = callback_copy["data"].as_str().unwrap_or("");
        let mut post_id = None;
        let callbacks = if let Some(post) = find_message(context, message)? {
            db::bump_click(context, post["id"].as_str().unwrap_or(""), data)?;
            post_id = post["id"].as_str().map(str::to_owned);
            post["payload"]["callbacks"].clone()
        } else if let (Some(chat), Some(mid)) = (
            message["chat"]["id"].as_i64(),
            message["message_id"].as_i64(),
        ) {
            preview(context, chat, mid)?
        } else {
            Value::Null
        };
        let answer = &callbacks[data];
        let mut params = json!({});
        if answer.is_string() {
            params["text"] = answer.clone();
        } else if let Some(answer) = answer.as_object() {
            for (key, value) in answer {
                if key != "reply" && key != "callback_query_id" {
                    params[key] = value.clone();
                }
            }
        }
        Ok((
            params,
            answer.get("reply").cloned().filter(Value::is_object),
            post_id,
        ))
    })
    .await;
    let (mut params, reply, post_id) = match lookup {
        Ok(result) => result,
        Err(error) => {
            state.logs.event(
                "ERROR",
                "rich-studio.callbacks",
                &error.to_string(),
                json!({"event":"callback.lookup_failed"}),
            );
            (json!({}), None, None)
        }
    };
    params["callback_query_id"] = callback["id"].clone();
    if let Some(reply) = reply {
        let received = callback.get("_studio_received").and_then(Value::as_u64);
        let age = received.map_or(0, |received| {
            crate::store::auth::now()
                .saturating_sub(received as i64)
                .max(0) as u64
        });
        let wait = Duration::from_secs(15u64.saturating_sub(age).min(4));
        let report = {
            let spec = state.spec.read().await;
            crate::spec::validator::Validator::new(&spec).check_reply(&reply)
        };
        let result = if report["ok"] != true {
            Err(format!(
                "rich-ответ не прошёл проверку текущей спецификации: {}",
                report["errors"]
            ))
        } else if age > 14 {
            Err("нажатие ждало больше 14 секунд: rich-ответ не отправлен".to_owned())
        } else {
            match tokio::time::timeout(
                wait,
                send_reply(state, context, callback["id"].as_str().unwrap_or(""), reply),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(
                    "Telegram не ответил вовремя: rich-ответ мог дойти, повтор не выполняется"
                        .to_owned(),
                ),
            }
        };
        if let Err(error) = result {
            let error = crate::logs::scrub(&state.settings, &error);
            let data = callback["data"].as_str().unwrap_or("").to_owned();
            state.logs.event(
                "WARNING",
                "rich-studio.callbacks",
                &error,
                json!({"event":"callback.reply_failed","post_id":post_id,"callback_data":data}),
            );
            if let Some(id) = post_id
                && let Err(error) = blocking::run(context, move |context| {
                    Ok(crate::store::stats::reply_error(
                        context, &id, &data, &error,
                    )?)
                })
                .await
            {
                state.logs.event(
                    "ERROR",
                    "rich-studio.callbacks",
                    &error.to_string(),
                    json!({"event":"callback.reply_record_failed"}),
                );
            }
        } else {
            state.logs.event("INFO","rich-studio.callbacks","rich-ответ на кнопку отправлен",json!({"event":"callback.reply","post_id":post_id,"callback_data":callback["data"]}));
        }
    }
    if let Err(error) = transport(state)
        .call(context, "answerCallbackQuery", params, Uploads::default())
        .await
        && !error.to_string().contains("query is too old")
    {
        return Err(error.into());
    }
    Ok(())
}
pub(super) async fn content_callback(
    state: &AppState,
    update: &Value,
) -> Result<bool, ServiceError> {
    if !update["callback_query"].is_object() {
        return Ok(false);
    }
    let Some(uid) = bot_dialog::private_sender(update) else {
        return Ok(false);
    };
    let Some(mid) = update["callback_query"]["message"]["message_id"].as_i64() else {
        return Ok(false);
    };
    let settings = state.settings.clone();
    let result = blocking::run(&AccountContext::root(&settings.clone()), move |_| {
        let context = if settings.public && auth::account(&settings, uid)?.is_some() {
            bot_dialog::context(&settings, uid)
        } else if settings.preview_chat == uid.to_string() {
            AccountContext::root(&settings)
        } else {
            return Ok(false);
        };
        Ok(db::find_post(&context, uid, mid)?.is_some()
            || has_callbacks(&preview(&context, uid, mid)?))
    })
    .await;
    match result {
        Ok(found) => Ok(found),
        Err(error) => {
            state.logs.event(
                "ERROR",
                "rich-studio.callbacks",
                &error.to_string(),
                json!({"event":"callback.lookup_failed"}),
            );
            // A lookup failure must not reinterpret content callback_data as a bot command.
            // Scoped handling can still acknowledge the press without sending rich content.
            Ok(true)
        }
    }
}
