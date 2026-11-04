use super::{
    files::{SavedMedia, download_document, download_photo},
    normalize::{mt_entities, referenced_media, tl_value},
};
use crate::{
    account_context::AccountContext,
    errors::ImportError,
    settings::Settings,
    spec::import_content::{Converter, entities_text},
};
use grammers_client::Client;
use grammers_tl_types as tl;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub async fn message_blocks(
    client: &Client,
    context: &AccountContext,
    settings: &Settings,
    message: &tl::types::Message,
    saved: &mut SavedMedia,
    warnings: &mut Vec<String>,
) -> Result<Vec<Value>, ImportError> {
    if message.noforwards || message.ttl_period.is_some_and(|ttl| ttl != 0) {
        return Err(ImportError::new(
            403,
            "Импорт защищённых или исчезающих сообщений недоступен.",
        ));
    }
    if let Some(tl::enums::RichMessage::Message(rich)) = &message.rich_message {
        if rich.part {
            return Err(ImportError::new(
                502,
                "Telegram вернул только часть rich-поста. Импорт отменён.",
            ));
        }
        let data = tl_value(&rich.blocks, "PageBlock")?;
        let mut needed = BTreeSet::new();
        referenced_media(&data, &mut needed);
        let mut media = BTreeMap::new();
        for photo in &rich.photos {
            if let tl::enums::Photo::Photo(photo) = photo
                && needed.contains(&photo.id.to_string())
            {
                media.insert(
                    photo.id.to_string(),
                    download_photo(client, context, settings, saved, photo.clone().into()).await?,
                );
            }
        }
        for document in &rich.documents {
            if let tl::enums::Document::Document(document) = document
                && needed.contains(&document.id.to_string())
            {
                media.insert(
                    document.id.to_string(),
                    download_document(client, context, settings, saved, document.clone()).await?,
                );
            }
        }
        let mut converter = Converter::new(media);
        let blocks = converter.blocks(data.as_array().map(Vec::as_slice).unwrap_or(&[]))?;
        warnings.extend(converter.warnings);
        return Ok(blocks);
    }
    let mut blocks = Vec::new();
    if !message.message.is_empty() {
        let entities = mt_entities(message.entities.as_deref().unwrap_or(&[]))?;
        blocks.push(
            json!({"type":"paragraph","text":entities_text(&message.message,&entities,warnings)?}),
        );
    }
    let media = match &message.media {
        Some(tl::enums::MessageMedia::Photo(photo)) => {
            if photo.ttl_seconds.is_some_and(|ttl| ttl != 0) {
                return Err(ImportError::new(403, "Импорт исчезающих медиа недоступен."));
            }
            match &photo.photo {
                Some(value @ tl::enums::Photo::Photo(_)) => Some((
                    download_photo(client, context, settings, saved, value.clone()).await?,
                    photo.spoiler,
                )),
                _ => None,
            }
        }
        Some(tl::enums::MessageMedia::Document(document)) => {
            if document.ttl_seconds.is_some_and(|ttl| ttl != 0) {
                return Err(ImportError::new(403, "Импорт исчезающих медиа недоступен."));
            }
            match &document.document {
                Some(tl::enums::Document::Document(value)) => Some((
                    download_document(client, context, settings, saved, value.clone()).await?,
                    document.spoiler,
                )),
                _ => None,
            }
        }
        None | Some(tl::enums::MessageMedia::Empty) | Some(tl::enums::MessageMedia::WebPage(_)) => {
            None
        }
        Some(_) => {
            warnings.push("Неподдерживаемое вложение исходного сообщения не перенесено.".into());
            blocks.push(json!({"type":"paragraph","text":"[Неподдерживаемое вложение Telegram]"}));
            None
        }
    };
    if let Some((mut item, spoiler)) = media {
        let kind = item["type"].as_str().unwrap_or("document").to_owned();
        if spoiler {
            item["has_spoiler"] = json!(true);
        }
        let mut block = json!({"type":kind});
        block[&kind] = item;
        blocks.insert(0, block);
    }
    Ok(blocks)
}

pub fn full_rich_message(
    response: tl::enums::messages::Messages,
    id: i32,
) -> Result<tl::types::Message, ImportError> {
    let messages = match response {
        tl::enums::messages::Messages::Messages(value) => value.messages,
        tl::enums::messages::Messages::Slice(value) => value.messages,
        tl::enums::messages::Messages::ChannelMessages(value) => value.messages,
        tl::enums::messages::Messages::NotModified(_) => vec![],
    };
    messages.into_iter().find_map(|message|match message{tl::enums::Message::Message(value)if value.id==id&&matches!(&value.rich_message,Some(tl::enums::RichMessage::Message(rich))if !rich.part)=>Some(value),_=>None}).ok_or_else(||ImportError::new(502,"Telegram вернул только часть rich-поста. Импорт отменён."))
}
