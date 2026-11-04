//! Private bot commands are scoped by the verified Telegram sender, never by text.
use super::blocking;
use crate::{
    AppState,
    account_context::{AccountContext, Role},
    errors::ServiceError,
    settings::Settings,
    store::{auth, bot_dialog as db, stats},
    telegram::{client::TelegramTransport, uploads::Uploads},
};
use serde_json::{Value, json};

pub const PREFIX: &str = "rsbot:";
pub fn phrase<'a>(language: &str, ru: &'a str, en: &'a str) -> &'a str {
    if language == "ru" { ru } else { en }
}
pub fn recipient(context: &AccountContext, settings: &Settings) -> Option<i64> {
    context.account_id.or_else(|| {
        settings.preview_chat.parse::<i64>().ok().filter(|id| {
            *id > 0
                && *id < (1_i64 << 52)
                && settings.preview_chat.bytes().all(|c| c.is_ascii_digit())
        })
    })
}
pub fn context(settings: &Settings, uid: i64) -> AccountContext {
    if settings.public {
        AccountContext::account(settings, uid, Role::Publish)
    } else {
        AccountContext::root(settings)
    }
}
pub fn keyboard(
    settings: &Settings,
    language: &str,
    preferences: bool,
    link: Option<&str>,
) -> Value {
    let mut rows = Vec::new();
    if let Some(link) = link {
        rows.push(json!([{"text":phrase(language,"Посмотреть пост ↗","View post ↗"),"url":link}]));
    }
    if settings.public_url.starts_with("https://") {
        rows.push(json!([{"text":phrase(language,"Открыть студию ↗","Open studio ↗"),"url":settings.public_url}]));
    }
    if preferences {
        for (mode, ru, en) in [
            (
                "all",
                "🔔 Все публикации и ошибки",
                "🔔 All posts and errors",
            ),
            ("errors", "⚠️ Только ошибки", "⚠️ Errors only"),
            ("off", "🔕 Выключить", "🔕 Turn off"),
        ] {
            rows.push(json!([{"text":phrase(language,ru,en),"callback_data":format!("{PREFIX}notifications {mode}")}]));
        }
    } else {
        rows.push(json!([{"text":phrase(language,"Помощь","Help"),"callback_data":format!("{PREFIX}help")},{"text":phrase(language,"Уведомления","Notifications"),"callback_data":format!("{PREFIX}notifications")}]));
    }
    json!({"inline_keyboard":rows})
}
pub async fn send_private(
    context: &AccountContext,
    settings: &Settings,
    transport: &TelegramTransport,
    uid: i64,
    text: &str,
    markup: Value,
) -> Result<(), ServiceError> {
    if recipient(context, settings) != Some(uid) {
        return Err(ServiceError::BadRequest(
            "Получатель сообщения не совпадает с текущей учёткой.".to_owned(),
        ));
    }
    transport
        .send_studio_private(context, uid, crate::logs::scrub(settings, text), markup)
        .await?;
    Ok(())
}
pub async fn register_commands(state: &AppState) -> Result<(), ServiceError> {
    let transport = state.transport();
    for language in ["", "ru", "en"] {
        let commands=[("start","Знакомство со студией","Welcome to the studio"),("help","Как создавать и публиковать посты","How to create and publish posts"),("studio","Открыть редактор","Open the editor"),("status","Мои публикации и очередь","My posts and publishing queue"),("notifications","Настроить уведомления","Notification settings"),("stop","Выключить уведомления","Turn off notifications")].map(|(command,ru,en)|json!({"command":command,"description":if language=="en"{en}else{ru}}));
        transport.call(&AccountContext::root(&state.settings),"setMyCommands",json!({"scope":{"type":"all_private_chats"},"language_code":language,"commands":commands}),Uploads::default()).await?;
    }
    Ok(())
}
fn language(sender: &Value) -> &'static str {
    if sender["language_code"]
        .as_str()
        .unwrap_or("ru")
        .starts_with("en")
    {
        "en"
    } else {
        "ru"
    }
}
pub fn private_sender(update: &Value) -> Option<i64> {
    let callback = &update["callback_query"];
    let message = if callback.is_object() {
        &callback["message"]
    } else {
        &update["message"]
    };
    let sender = if callback.is_object() {
        &callback["from"]
    } else {
        &message["from"]
    };
    let uid = sender["id"].as_i64()?;
    (uid > 0
        && uid < (1_i64 << 52)
        && sender["is_bot"].as_bool() != Some(true)
        && message["chat"]["type"].as_str() == Some("private")
        && message["chat"]["id"].as_i64() == Some(uid))
    .then_some(uid)
}
async fn status(settings: &Settings, uid: i64, language: &str) -> Result<String, ServiceError> {
    let settings_copy = settings.clone();
    let known = blocking::run(&AccountContext::root(settings), move |_| {
        Ok(if settings_copy.public {
            auth::account(&settings_copy, uid)?.is_some()
        } else {
            settings_copy.preview_chat == uid.to_string()
        })
    })
    .await?;
    if !known {
        return Ok(phrase(language,"Сначала откройте студию и войдите через Telegram. Здесь появится состояние ваших публикаций.","Open the studio and sign in with Telegram first. Your publishing status will appear here.").to_owned());
    }
    let counts = blocking::run(&context(settings, uid), |context| {
        Ok(stats::post_counts(context)?)
    })
    .await?;
    let mut text = format!(
        "<b>{}</b>\n\n",
        phrase(language, "📊 Ваши публикации", "📊 Your posts")
    );
    for (key, ru, en) in [
        ("sent", "Опубликовано", "Published"),
        ("scheduled", "В очереди", "Scheduled"),
        ("sending", "Отправляется", "Sending"),
        ("failed", "Требуют проверки", "Need attention"),
        ("missed", "Пропущено", "Missed"),
    ] {
        text.push_str(&format!(
            "{}: {}\n",
            phrase(language, ru, en),
            counts[key].as_i64().unwrap_or(0)
        ));
    }
    text.push_str(phrase(
        language,
        "\nПодробности — во вкладке «Посты» в студии.",
        "\nSee the Posts tab for details.",
    ));
    Ok(text)
}
fn help(language: &str) -> &str {
    phrase(
        language,
        "<b>Как работать с Rich Studio</b>\n\n1. Откройте студию и войдите через Telegram.\n2. Добавьте бота администратором канала с правом публикации и подключите канал в окне «Каналы».\n3. Создайте пост. «Мне в личку» покажет предпросмотр, «Опубликовать» отправит в канал.\n4. Можно назначить время публикации — студия должна работать в этот момент.\n\n/studio — открыть редактор\n/status — ваши публикации и очередь\n/notifications — все уведомления, только ошибки или тишина\n/stop — выключить уведомления\n\nЕсли бот пишет «мог уйти», проверьте канал перед повтором. Пересылку для импорта начните в студии.",
        "<b>Getting started with Rich Studio</b>\n\n1. Open the studio and sign in with Telegram.\n2. Add the bot as a channel administrator with posting rights, then connect the channel in Channels.\n3. Create a post. Send a private preview to check it, then Publish to your channel.\n4. You can schedule a post — the studio must be running at that time.\n\n/studio — open the editor\n/status — your posts and queue\n/notifications — all notifications, errors only, or off\n/stop — turn notifications off\n\nIf a post may have been sent, check the channel before retrying. Start forward imports in the studio.",
    )
}
pub async fn handle(state: &AppState, update: &Value) -> Result<bool, ServiceError> {
    let Some(uid) = private_sender(update) else {
        return Ok(false);
    };
    if !state.settings.public && state.settings.preview_chat != uid.to_string() {
        return Ok(false);
    }
    let callback = &update["callback_query"];
    let is_callback = callback.is_object();
    let message = if is_callback {
        &callback["message"]
    } else {
        &update["message"]
    };
    let sender = if is_callback {
        &callback["from"]
    } else {
        &message["from"]
    };
    if !is_callback && !message["forward_origin"].is_null() {
        return Ok(false);
    }
    let raw = if is_callback {
        callback["data"]
            .as_str()
            .and_then(|text| text.strip_prefix(PREFIX))
    } else {
        message["text"].as_str()
    };
    let Some(raw) = raw.filter(|raw| !raw.trim().is_empty()) else {
        return Ok(false);
    };
    let mut parts = raw.trim().splitn(2, char::is_whitespace);
    let raw_command = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    let command = if is_callback {
        raw_command.to_owned()
    } else {
        let parsed = raw_command
            .strip_prefix('/')
            .map(|s| s.split_once('@').unwrap_or((s, "")));
        match parsed {
            Some((command, bot))
                if !command.is_empty()
                    && command
                        .bytes()
                        .all(|c| c.is_ascii_alphabetic() || c == b'_') =>
            {
                if !bot.is_empty() && !bot.eq_ignore_ascii_case(&state.settings.bot_username) {
                    return Ok(false);
                }
                command.to_ascii_lowercase()
            }
            _ => "unknown".to_owned(),
        }
    };
    let language = language(sender);
    let mode = if command == "stop" {
        Some("off")
    } else if command == "notifications" && ["all", "errors", "off"].contains(&arg) {
        Some(arg)
    } else {
        None
    };
    let start = command == "start" && !arg.starts_with("import_");
    let settings = state.settings.clone();
    let mode_owned = mode.map(str::to_owned);
    let (allowed, prefs) = blocking::run(&AccountContext::root(&settings.clone()), move |_| {
        if mode_owned.as_deref() == Some("off") {
            db::disable(&settings, uid)?;
        }
        let allowed = auth::take_budget(&settings, &format!("bot-dialog:{uid}"), 12, 60)?;
        let existing = db::contact(&settings, uid)?;
        let prefs = if allowed && (start || mode_owned.is_some() || existing.is_some()) {
            db::touch(&settings, uid, language, mode_owned.as_deref(), start)?
        } else {
            existing.unwrap_or_else(|| json!({"mode":"off"}))
        };
        Ok((allowed, prefs))
    })
    .await?;
    let transport = state.transport();
    if is_callback {
        transport
            .call(
                &state.account,
                "answerCallbackQuery",
                json!({"callback_query_id":callback["id"]}),
                Uploads::default(),
            )
            .await?;
    }
    if !allowed {
        return Ok(true);
    }
    let mut text=match command.as_str(){
        "start" if arg.starts_with("import_")=>phrase(language,"Запрос импорта устарел или не найден. Начните импорт заново в студии и откройте новую ссылку.","This import request expired or was not found. Start a new import in the studio and open its link.").to_owned(),
        "start"=>phrase(language,"<b>Привет! Я бот Rich Studio 👋</b>\n\nПомогаю публиковать красивые посты в Telegram: таблицы, фото, кнопки и отложенные публикации.\n\nОткройте студию, войдите через Telegram и подключите свой канал. Здесь будут предпросмотры и отчёты о публикациях.\n\n/help — с чего начать\n/notifications — настроить уведомления","<b>Hi! I'm the Rich Studio bot 👋</b>\n\nCreate Telegram posts with tables, photos, buttons and scheduled publishing.\n\nOpen the studio, sign in with Telegram and connect your channel. Your previews and publishing reports will arrive here.\n\n/help — getting started\n/notifications — notification settings").to_owned(),
        "help"=>help(language).to_owned(),"status"=>status(&state.settings,uid,language).await?,
        "studio"=>phrase(language,"🎨 Откройте студию, чтобы создать или отредактировать пост.","🎨 Open the studio to create or edit a post.").to_owned(),
        "notifications"|"stop"=>{
            let labels=match prefs["mode"].as_str().unwrap_or("off"){"all"=>("все публикации и ошибки","all posts and errors"),"errors"=>("только ошибки","errors only"),_=>("выключены","off")};
            format!("{}{}{}",phrase(language,"🔔 Уведомления: ","🔔 Notifications: "),phrase(language,labels.0,labels.1),phrase(language,"\n\nВыберите режим ниже. /stop в любой момент выключит уведомления.","\n\nChoose a mode below. Use /stop at any time to turn notifications off."))
        },_=>phrase(language,"Посты создаются в студии. Я помогу с предпросмотром и сообщу о публикациях. Нажмите «Открыть студию» или отправьте /help.","Create posts in the studio. I handle previews and publishing reports. Tap Open studio or send /help.").to_owned()
    };
    if !state.settings.public_url.starts_with("https://") {
        text.push_str(&format!(
            "\n\n{}",
            html_escape::encode_text(&state.settings.public_url)
        ));
    }
    send_private(
        &context(&state.settings, uid),
        &state.settings,
        &transport,
        uid,
        &text,
        keyboard(
            &state.settings,
            language,
            ["notifications", "stop"].contains(&command.as_str()),
            None,
        ),
    )
    .await?;
    Ok(true)
}
pub async fn import_reply(state: &AppState, update: &Value) -> Result<(), ServiceError> {
    let Some(uid) = private_sender(update) else {
        return Ok(());
    };
    let language = language(&update["message"]["from"]);
    let text = if update["message"]["forward_origin"].is_null() {
        phrase(
            language,
            "📥 Теперь перешлите сюда исходный пост из Telegram.",
            "📥 Now forward the original Telegram post here.",
        )
    } else {
        phrase(
            language,
            "📥 Пересылка обработана. Результат импорта или объяснение ошибки — в студии.",
            "📥 Forward processed. Open the studio for the imported draft or error details.",
        )
    };
    let transport = state.transport();
    send_private(
        &context(&state.settings, uid),
        &state.settings,
        &transport,
        uid,
        text,
        keyboard(&state.settings, language, false, None),
    )
    .await
}
