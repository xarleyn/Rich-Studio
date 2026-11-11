use super::PREVIEW_KEY;
use crate::{
    account_context::AccountContext, errors::ServiceError, services::blocking, store::meta,
};
use serde::Deserialize;
use serde_json::{Value, json};

// Deserialize legacy JSON object insertion order without changing serde_json semantics globally.
struct Ordered(Vec<(String, Value)>);
impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("preview callback object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Ordered, M::Error> {
                let mut rows = Vec::new();
                while let Some(row) = map.next_entry()? {
                    rows.push(row);
                }
                Ok(Ordered(rows))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}
pub async fn remember_preview(
    context: &AccountContext,
    chat: &Value,
    message: i64,
    callbacks: &Value,
) -> Result<(), ServiceError> {
    let key = format!(
        "{}:{message}",
        chat.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| chat.to_string())
    );
    let callbacks = callbacks.clone();
    blocking::run(context, move |context| {
        let raw = meta::get(context, PREVIEW_KEY)?.unwrap_or_else(|| "{}".to_owned());
        let mut rows = serde_json::from_str::<Ordered>(&raw)
            .map_err(|e| ServiceError::Internal(e.to_string()))?
            .0;
        if let Some(row) = rows.iter_mut().find(|(saved, _)| *saved == key) {
            row.1 = callbacks;
        } else {
            rows.push((key, callbacks));
        }
        let keep = rows.len().saturating_sub(20);
        rows.drain(..keep);
        // Explicit serialization preserves the legacy object order for the next append.
        let fields = rows
            .into_iter()
            .map(|(key, value)| format!("{}:{value}", json!(key)))
            .collect::<Vec<_>>()
            .join(",");
        meta::set(context, PREVIEW_KEY, &format!("{{{fields}}}"))?;
        Ok(())
    })
    .await
}
pub(super) fn preview(
    context: &AccountContext,
    chat: i64,
    message: i64,
) -> Result<Value, ServiceError> {
    let saved: Value =
        serde_json::from_str(&meta::get(context, PREVIEW_KEY)?.unwrap_or_else(|| "{}".to_owned()))
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
    Ok(saved
        .get(format!("{chat}:{message}"))
        .cloned()
        .unwrap_or_else(|| json!({})))
}
pub(super) fn has_callbacks(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}
