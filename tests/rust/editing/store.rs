use rich_studio::account_context::AccountContext;
use rich_studio::services::listing;
use rich_studio::store::{editing, posts};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{add, config, post};

#[test]
fn claims_are_atomic_and_scheduler_race_cannot_replace_in_flight_content()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let settings = config(&dir);
    let context = AccountContext::root(&settings);
    let old = post("Old");
    let record = add(&context, "scheduled", &old);
    let id = record["id"].as_str().ok_or("id")?;
    assert_eq!(editing::claim_version(&context, id, Some(0))?, Some(1));
    assert_eq!(editing::claim_version(&context, id, Some(0))?, None);
    assert_eq!(editing::claim_version(&context, id, None)?, Some(2));
    editing::release_version(&context, id, 1)?;
    assert_eq!(posts::get(&context, id)?.ok_or("post")?["version"], 2);
    assert_eq!(editing::claim_version(&context, "nope", None)?, None);
    rich_studio::store::Store::connect(&context)?
        .execute("update posts set status='sending' where id=?", [id])?;
    assert!(!editing::replace_scheduled(
        &context,
        id,
        &post("New"),
        &old,
        "api",
        1
    )?);
    assert_eq!(posts::get(&context, id)?.ok_or("post")?["payload"], old);
    assert!(editing::versions(&context, id)?.is_empty());
    Ok(())
}

#[test]
fn listing_preserves_titles_thumbnails_buttons_and_links() {
    assert_eq!(
        listing::title(
            &json!({"rich_message":{"html":"<h1>Привет &amp; &#x1f680;</h1><p>Вторая строка</p>"}}),
            Some("rich")
        )
        .unwrap_or_default(),
        "Привет & 🚀"
    );
    assert_eq!(
        listing::title(
            &json!({"rich_message":{"markdown":"## **Заголовок** [ссылка](https://example.org)"}}),
            Some("rich")
        )
        .unwrap_or_default(),
        "Заголовок ссылка"
    );
    let payload = json!({"live_photo":{"photo":"upload:still.jpg","caption":"Caption","reply_markup":{"inline_keyboard":[[{"text":"Go","callback_data":"cb"}]]}}});
    assert_eq!(
        listing::thumb(&payload),
        Some("/media/still.jpg".to_owned())
    );
    assert_eq!(
        listing::button_texts(&payload, Some("live_photo")).unwrap_or(Value::Null),
        json!({"cb":"Go"})
    );
    assert_eq!(
        listing::button_texts(
            &json!({"reply_markup":{"inline_keyboard":[[{"text":"Да","callback_data":"yes"}]]}}),
            None,
        )
        .unwrap_or(Value::Null),
        json!({"yes":"Да"})
    );
    assert_eq!(
        listing::title(&json!({"poll":{"question":"Как?"}}), Some("poll")).unwrap_or_default(),
        "Как?"
    );
    assert_eq!(
        listing::title(&json!({"paid_media":{}}), Some("paid_media")).unwrap_or_default(),
        "Платный контент"
    );
    assert_eq!(
        listing::link(&json!("-1001234"), &json!(8)),
        Some("https://t.me/c/1234/8".to_owned())
    );
}
