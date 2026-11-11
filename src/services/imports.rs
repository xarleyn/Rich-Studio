//! Account-scoped import into a new editable draft; this module never publishes.

mod draft;
mod files;
mod forward;
mod message;
mod mtproto;
mod normalize;
mod session;

use self::mtproto::mtproto;
use super::{import_links::parse_link, import_web};
use crate::{account_context::AccountContext, errors::ImportError, settings::Settings};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;

pub use self::{
    draft::{save_import, save_result},
    files::{SavedMedia, document_kind, save_download},
    forward::from_forward,
    message::{full_rich_message, message_blocks},
    normalize::{mt_entities, normalize_tl},
    session::{ActiveImport, session_path, status},
};

pub const MAX_FILES: usize = 30;
pub const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
pub const DEADLINE: Duration = Duration::from_secs(120);

pub async fn import_post(
    settings: &Settings,
    context: &AccountContext,
    url: &str,
) -> Result<Value, ImportError> {
    let link = parse_link(url)?;
    let _active = ActiveImport::acquire(context.account_id)?;
    let mut saved = SavedMedia::default();
    let read = async {
        if settings.import_api_id != 0 && !settings.import_api_hash.is_empty() {
            let (blocks, warnings) = mtproto(context, settings, &link, &mut saved).await?;
            return Ok((blocks, warnings, "mtproto"));
        }
        let widget = import_web::fetch_widget(&link).await?;
        if widget.media_urls.len() > MAX_FILES {
            return Err(ImportError::new(
                413,
                "В одном импорте допускается не больше 30 файлов.",
            ));
        }
        let mut blocks = if widget.root.is_empty() {
            Vec::new()
        } else {
            vec![json!({"type":"paragraph","text":widget.root})]
        };
        for url in widget.media_urls {
            let response = import_web::fetch_media(&url).await?;
            let declared = response.content_length().unwrap_or(0);
            let chunks = response.bytes_stream().map(|result| {
                result
                    .map(|bytes| bytes.to_vec())
                    .map_err(|_| ImportError::new(502, "Не удалось скачать медиа Telegram."))
            });
            let item = save_download(
                context, settings, &mut saved, "photo", None, declared, chunks,
            )
            .await?;
            blocks.push(json!({"type":"photo","photo":item}));
        }
        let mut warnings =
            vec!["Импорт из веб-виджета: проверьте полноту содержимого и медиа.".into()];
        warnings.extend(widget.warnings);
        Ok((blocks, warnings, "web"))
    };
    let (blocks, warnings, provider) = tokio::time::timeout(DEADLINE, read)
        .await
        .map_err(|_| ImportError::new(502, "Время импорта Telegram истекло."))??;
    save_import(context.clone(), blocks, warnings, link, provider, saved).await
}
