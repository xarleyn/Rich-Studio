use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rich_studio::account_context::AccountContext;
use rich_studio::errors::{ServiceError, StoreError};
use rich_studio::services::idempotency::{Idempotency, fingerprint};
use rich_studio::settings::Settings;
use rich_studio::store::db::Store;
use rich_studio::telegram::TelegramError;
use serde_json::{Value, json};
use tempfile::TempDir;

fn context(dir: &TempDir) -> AccountContext {
    let settings = Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        dir.path().to_string_lossy().into_owned(),
    )]))
    .unwrap_or_else(|error| panic!("settings: {error}"));
    AccountContext::root(&settings)
}

#[test]
fn fingerprints_match_python_fixtures() {
    let fixtures = [
        (
            "send",
            json!({"post":{"rich_message":{"markdown":"Привет, ё 🚀"}},"chat_ids":["@a","-100"]}),
            "ca3c25f50226e7d5eb980ae70088a0e35fbedfed926ce249455de852b17d4329",
        ),
        (
            "media-post",
            json!({"kind":"paid_media","params":{"media":[{"type":"photo","media":"upload:кот.png","future":9007199254740993u64}],"caption":"Вложение"},"pin":false}),
            "06d186af2d06c5c21c0b9258dd31e81aebf61e34122728477d848ccf2187b1ca",
        ),
        (
            "send",
            json!({"post":{"rich_message":{"markdown":"цена"}},"price":"1.20"}),
            "e92dbfd29875b8bc9611843f8dbadeba4b84d976a3ac70b66a23ed1912c1dd02",
        ),
    ];
    for (scope, body, expected) in fixtures {
        assert_eq!(fingerprint(scope, &body), expected);
    }
}

#[tokio::test]
async fn done_uncertain_refusal_and_internal_errors_follow_key_policy()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let idempotency = Idempotency::new(context(&dir));
    let body = json!({"post":{"rich_message":{"markdown":"Привет"}}});
    let calls = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        let action_calls = Arc::clone(&calls);
        let result = idempotency
            .run(Some("done"), "send", &body, "api", move || async move {
                action_calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"ok":true}))
            })
            .await?;
        assert_eq!(result, json!({"ok":true}));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    let mismatch = idempotency
        .run(
            Some("done"),
            "send",
            &json!({"other":true}),
            "api",
            || async { Ok(Value::Null) },
        )
        .await;
    assert!(matches!(mismatch, Err(ServiceError::Unprocessable(_))));

    let calls = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        let calls = Arc::clone(&calls);
        let result = idempotency
            .run(
                Some("uncertain"),
                "send",
                &body,
                "api",
                move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(ServiceError::MayHaveGone(json!("response lost")))
                },
            )
            .await;
        assert!(matches!(result, Err(ServiceError::MayHaveGone(_))));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let calls = Arc::new(AtomicUsize::new(0));
    for attempt in 0..2 {
        let calls = Arc::clone(&calls);
        let result = idempotency
            .run(Some("refused"), "send", &body, "api", move || async move {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(ServiceError::Telegram(TelegramError::Refused {
                        description: "chat not found".to_owned(),
                        code: Some(400),
                        raw: None,
                    }))
                } else {
                    Ok(json!({"attempt":attempt}))
                }
            })
            .await;
        if attempt == 0 {
            assert!(matches!(result, Err(ServiceError::Telegram(_))));
        } else {
            assert_eq!(result?, json!({"attempt":1}));
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    for (key, error) in [
        ("internal", ServiceError::Internal("disk full".to_owned())),
        ("quota", ServiceError::Store(StoreError::StorageFull)),
    ] {
        let result = idempotency
            .run(Some(key), "send", &body, "api", || async { Err(error) })
            .await;
        assert!(result.is_err());
        let result = idempotency
            .run(Some(key), "send", &body, "api", || async {
                Ok(json!({"duplicate":true}))
            })
            .await;
        assert!(matches!(result, Err(ServiceError::MayHaveGone(_))));
    }
    Ok(())
}

#[test]
fn python_float_notation_is_preserved_in_old_key_fingerprints() {
    for (value, expected) in [
        (
            1e-5,
            "f443430474bb15b1175cf38fa0ac724c601285b16653e12d8eb33ba9ba980b53",
        ),
        (
            1e-6,
            "ae87eabfba50d1ec1af6dd6c0d9b6035fa755b5afdc1216a22289b229dceb8ac",
        ),
        (
            1e-4,
            "daeeb06293e58ed38f1124d38c38a02ca224043acfdf231229cf72b076c77486",
        ),
        (
            1e16,
            "755c713a9a751d4d72b83978b3b06f8d634bbdeca44ac053b5291eea756262a8",
        ),
        (
            1e15,
            "db08ec8d5334be8337b4d3951e78a16969668254da908e62e1e4356556f917a9",
        ),
        (
            1e20,
            "5cc87ed0987793704c33da18ac1d6f7274755b86f048ab056b3a215e38fe2e4d",
        ),
        (
            1e30,
            "58b9b2b3ff8bceee6523b781a163fba69d5e8b8270e58cc69a55db64848a2cd7",
        ),
        (
            -0.0,
            "d68f66480b74e050393639e4e9bc5955560141bec3dc8020610bc06845467c98",
        ),
    ] {
        assert_eq!(
            fingerprint("send", &json!({"value":value})),
            expected,
            "{value}"
        );
    }
}

#[tokio::test]
async fn unresolved_keys_survive_retention_and_are_not_replayed()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    let body = json!({"value":1});
    for status in ["pending", "uncertain"] {
        rich_studio::store::idempotency::claim(
            &context,
            status,
            "send",
            &fingerprint("send", &body),
        )?;
        rich_studio::store::idempotency::finish(&context, status, status, &json!("lost response"))?;
        Store::connect(&context)?
            .execute("update idempotency set created_at=1 where key=?", [status])?;
        rich_studio::store::idempotency::purge(&context, None)?;
        let calls = Arc::new(AtomicUsize::new(0));
        let cloned = calls.clone();
        let result = Idempotency::new(context.clone())
            .run(Some(status), "send", &body, "api", move || async move {
                cloned.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Null)
            })
            .await;
        assert!(matches!(result, Err(ServiceError::MayHaveGone(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
    Ok(())
}

#[tokio::test]
async fn pending_keys_conflict_then_become_uncertain_and_long_keys_never_run()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = TempDir::new()?;
    let context = context(&dir);
    let idempotency = Idempotency::new(context.clone());
    let body = json!({"x":1});
    assert!(
        rich_studio::store::idempotency::claim(
            &context,
            "pending",
            "send",
            &fingerprint("send", &body)
        )?
        .is_none()
    );
    let result = idempotency
        .run(Some("pending"), "send", &body, "api", || async {
            Ok(Value::Null)
        })
        .await;
    assert!(matches!(result, Err(ServiceError::Conflict(_))));
    let connection = Store::connect(&context)?;
    connection.execute(
        "update idempotency set created_at=created_at-1000 where key='pending'",
        [],
    )?;
    let result = idempotency
        .run(Some("pending"), "send", &body, "api", || async {
            Ok(Value::Null)
        })
        .await;
    assert!(matches!(result, Err(ServiceError::MayHaveGone(_))));

    let key = "k".repeat(201);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    let result = idempotency
        .run(Some(&key), "send", &body, "api", move || async move {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        })
        .await;
    assert!(matches!(result, Err(ServiceError::BadRequest(_))));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}
