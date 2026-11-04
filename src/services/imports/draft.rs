use super::files::SavedMedia;
use crate::{
    account_context::AccountContext,
    errors::ImportError,
    services::import_links::MessageLink,
    spec::import_content::representations,
    store::{db, drafts},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn save_result(
    context: &AccountContext,
    blocks: Vec<Value>,
    mut warnings: Vec<String>,
    link: &MessageLink,
    provider: &str,
    title: &str,
) -> Result<Value, ImportError> {
    if blocks.is_empty() {
        return Err(ImportError::new(
            422,
            "Сообщение не содержит поддерживаемого содержимого.",
        ));
    }
    let encoded = crate::store::templates::encode(&json!(blocks))
        .map_err(|_| ImportError::new(422, "Неверное содержимое сообщения."))?;
    if encoded.len() > 2 * 1024 * 1024 {
        return Err(ImportError::new(
            413,
            "Содержимое превышает лимит импорта 2 МБ.",
        ));
    }
    let (html, markdown, files) = representations(&blocks, &mut warnings)?;
    let mut seen = BTreeSet::new();
    warnings.retain(|warning| seen.insert(warning.clone()));
    let mut source = link.describe();
    source["provider"] = json!(provider);
    let payload = json!({"rich_message":{"blocks":blocks,"media":files},"options":{},"_studio":{"format":"blocks","buffers":{"blocks":blocks,"html":html,"markdown":markdown},"import_source":source,"import_warnings":warnings}});
    let title = if title.is_empty() {
        format!(
            "Импорт {}/{}",
            link.username.as_deref().unwrap_or("Telegram"),
            link.message_id
        )
    } else {
        title.to_owned()
    };
    db::migrate(&mut db::Store::connect(context)?)?;
    let draft = drafts::save(context, None, &title, &payload, None, None, None)?
        .ok_or_else(|| ImportError::new(500, "Не удалось сохранить импорт."))?;
    Ok(
        json!({"draft":draft,"source":source,"warnings":warnings,"block_count":blocks.len(),"media_count":files.len()}),
    )
}

pub async fn save_import(
    context: AccountContext,
    blocks: Vec<Value>,
    warnings: Vec<String>,
    link: MessageLink,
    provider: &'static str,
    mut saved: SavedMedia,
) -> Result<Value, ImportError> {
    // The bounded blocking save owns cleanup. If an HTTP client disconnects at this point,
    // the worker finishes either the complete draft+files or cleanup, never a dangling draft.
    tokio::task::spawn_blocking(move || {
        let result = save_result(&context, blocks, warnings, &link, provider, "")?;
        saved.commit();
        Ok(result)
    })
    .await
    .map_err(|_| ImportError::new(500, "Сохранение импорта прервано."))?
}
