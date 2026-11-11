use super::{
    files::{SavedMedia, io_error},
    message::{full_rich_message, message_blocks},
    session::session_path,
};
use crate::{
    account_context::AccountContext,
    errors::ImportError,
    services::import_links::MessageLink,
    settings::Settings,
    telegram::import_client::{Connection, rpc_error},
};
use grammers_client::{Client, peer::Peer};
use grammers_session::types::PeerRef;
use grammers_tl_types as tl;
use serde_json::Value;

async fn resolve_peer(
    client: &Client,
    link: &MessageLink,
    is_bot: bool,
) -> Result<PeerRef, ImportError> {
    let peer = if let Some(username) = &link.username {
        client.resolve_username(username).await.map_err(rpc_error)?
    } else {
        let mut dialogs = client.iter_dialogs();
        let mut found = None;
        while let Some(dialog) = dialogs.next().await.map_err(rpc_error)? {
            if dialog.peer_id().bare_id() == link.channel_id {
                found = Some(dialog.peer().clone());
                break;
            }
        }
        found
    }
    .ok_or_else(|| ImportError::new(404, "Источник сообщения не найден."))?;
    let raw = match &peer {
        Peer::Channel(channel) => Some(&channel.raw),
        Peer::Group(group) => match &group.raw {
            tl::enums::Chat::Channel(channel) => Some(channel),
            _ => None,
        },
        _ => None,
    }
    .ok_or_else(|| ImportError::new(422, "Импорт поддерживает сообщения каналов и супергрупп."))?;
    if raw.noforwards {
        return Err(ImportError::new(
            403,
            "Импорт защищённого канала недоступен.",
        ));
    }
    if is_bot
        && link.username.as_ref().is_none_or(|username| {
            !peer
                .usernames()
                .iter()
                .any(|known| known.eq_ignore_ascii_case(username))
        })
    {
        return Err(ImportError::new(
            403,
            "Общее подключение импортирует только публичные каналы.",
        ));
    }
    peer.to_ref()
        .await
        .map_err(|_| ImportError::new(502, "Источник сообщения недоступен."))?
        .ok_or_else(|| ImportError::new(404, "Источник сообщения недоступен."))
}

pub(super) async fn mtproto(
    context: &AccountContext,
    settings: &Settings,
    link: &MessageLink,
    saved: &mut SavedMedia,
) -> Result<(Vec<Value>, Vec<String>), ImportError> {
    let path = session_path(settings, context);
    let session = match tokio::fs::read_to_string(path).await {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_error(error)),
    };
    if link.channel_id.is_some() && session.trim().is_empty() {
        return Err(ImportError::new(
            403,
            "Для приватной ссылки требуется собственное подключение Telegram.",
        ));
    }
    let connection = Connection::connect(settings, session.trim(), context.account_id).await?;
    let client = &connection.client;
    let peer = resolve_peer(client, link, connection.is_bot).await?;
    let original = client
        .get_messages_by_id(peer, &[link.message_id])
        .await
        .map_err(rpc_error)?
        .into_iter()
        .flatten()
        .find_map(|message| match message.raw {
            tl::enums::Message::Message(value) => Some(value),
            _ => None,
        })
        .ok_or_else(|| ImportError::new(404, "Сообщение не найдено."))?;
    if let Some(topic) = link.topic_id {
        let actual = match &original.reply_to {
            Some(tl::enums::MessageReplyHeader::Header(reply)) => {
                reply.reply_to_top_id.or(reply.reply_to_msg_id)
            }
            _ => None,
        };
        if actual != Some(topic) {
            return Err(ImportError::new(
                422,
                "Сообщение не принадлежит теме, указанной в ссылке.",
            ));
        }
    }
    let mut warnings = Vec::new();
    let messages = if original.grouped_id.is_some() && !link.single {
        let ids = ((original.id - 10).max(1)..=original.id.saturating_add(10)).collect::<Vec<_>>();
        let mut messages = client
            .get_messages_by_id(peer, &ids)
            .await
            .map_err(rpc_error)?
            .into_iter()
            .flatten()
            .filter_map(|message| match message.raw {
                tl::enums::Message::Message(value) if value.grouped_id == original.grouped_id => {
                    Some(value)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.id);
        warnings.push(
            "Импортированы доступные элементы альбома рядом с сообщением; проверьте комплектность."
                .into(),
        );
        messages
    } else {
        vec![original]
    };
    let mut blocks = Vec::new();
    for mut message in messages {
        if matches!(&message.rich_message,Some(tl::enums::RichMessage::Message(rich))if rich.part) {
            let response = client
                .invoke(&tl::functions::messages::GetRichMessage {
                    peer: peer.into(),
                    id: message.id,
                })
                .await
                .map_err(rpc_error)?;
            message = full_rich_message(response, message.id)?;
        }
        blocks.extend(
            message_blocks(client, context, settings, &message, saved, &mut warnings).await?,
        );
        if message.reply_markup.is_some() {
            warnings.push(
                "Кнопки исходного сообщения не перенесены: их действия принадлежат другому боту."
                    .into(),
            );
        }
    }
    connection.close().await;
    Ok((blocks, warnings))
}
