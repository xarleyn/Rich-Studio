//! Ссылки `upload:<name>` в постах, черновиках и шаблонах.
//!
//! Сырой SQL по трём таблицам: `used_by` — точное совпадение JSON-строки,
//! `upload_refs` — regex по всем payload. Вынос в `store` — отдельная задача.

use regex::Regex;
use serde_json::json;

use super::{MediaError, MediaService};
use crate::store::db::Store;

impl MediaService {
    pub fn used_by(&self, name: &str) -> Result<Vec<String>, MediaError> {
        // Match the JSON string, not a bare prefix: upload:cat.png must not claim upload:cat.png.bak.
        let reference = json!(format!("upload:{name}")).to_string();
        let mut connection = Store::connect(&self.context)?;
        crate::store::db::migrate(&mut connection)?;
        let mut output = Vec::new();
        let mut posts = connection.prepare(
            "select status,chat_id,payload from posts where status in ('scheduled','missed')",
        )?;
        let rows = posts.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (status, chat_id, payload) = row?;
            if payload.contains(&reference) {
                output.push(format!(
                    "{} пост в {chat_id}",
                    if status == "scheduled" {
                        "отложенный"
                    } else {
                        "пропущенный"
                    }
                ));
            }
        }
        drop(posts);
        let mut drafts = connection.prepare("select title,payload from drafts")?;
        let rows = drafts.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (title, payload) = row?;
            if payload.contains(&reference) {
                output.push(format!(
                    "черновик «{}»",
                    if title.is_empty() {
                        "Без названия"
                    } else {
                        &title
                    }
                ));
            }
        }
        drop(drafts);
        let mut templates =
            connection.prepare("select title,payload,fields from post_templates")?;
        let rows = templates.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (title, payload, fields) = row?;
            if payload.contains(&reference) || fields.contains(&reference) {
                output.push(format!("шаблон «{title}»"));
            }
        }
        Ok(output)
    }

    pub(super) fn upload_refs(&self) -> Result<std::collections::HashSet<String>, MediaError> {
        let mut connection = Store::connect(&self.context)?;
        crate::store::db::migrate(&mut connection)?;
        let pattern = Regex::new(r#""upload:([^"\\]+)""#)
            .unwrap_or_else(|error| panic!("upload regex: {error}"));
        let mut refs = std::collections::HashSet::new();
        for query in [
            "select payload from drafts",
            "select payload from post_templates",
            "select fields from post_templates",
            "select payload from posts where status in ('scheduled','sending','missed','sent')",
        ] {
            let mut statement = connection.prepare(query)?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                for capture in pattern.captures_iter(&row?) {
                    refs.insert(capture[1].to_owned());
                }
            }
        }
        Ok(refs)
    }
}
