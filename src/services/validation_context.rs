//! Resolve validation hints from the current account's checked channels only.

use serde_json::Value;

use crate::{AppState, spec::validator::Validator, store};

pub async fn check(
    state: &AppState,
    post: &Value,
    chat_ids: Option<Vec<String>>,
    draft_id: Option<String>,
) -> Result<Value, String> {
    let context = state.account.clone();
    let default_chat = state.settings.default_chat.clone();
    let targets = tokio::task::spawn_blocking(move || {
        let mut ids = chat_ids.unwrap_or_default();
        if ids.is_empty()
            && let Some(id) = draft_id
        {
            let Some(draft) = store::drafts::get(&context, &id)? else {
                return Ok(Vec::new());
            };
            ids = draft["chat_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        if ids.is_empty() && !default_chat.is_empty() {
            ids.push(default_chat);
        }
        let mut targets = Vec::new();
        for id in ids {
            if let Some(channel) = store::channels::get(&context, &Value::String(id.clone()))?
                && let Some(kind) = channel["type"].as_str().filter(|kind| !kind.is_empty())
            {
                let title = channel["title"]
                    .as_str()
                    .filter(|title| !title.is_empty())
                    .unwrap_or(&id);
                let target = (kind.to_owned(), title.to_owned());
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        Ok::<_, crate::errors::StoreError>(targets)
    })
    .await
    .map_err(|_| "проверка каналов прервана".to_owned())?
    .map_err(|error| error.to_string())?;
    let spec = state.spec.read().await;
    Ok(Validator::new(&spec).check_post_in(post, &targets))
}
