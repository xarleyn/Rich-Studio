//! Draft count and byte limits.

use axum::http::StatusCode;
use rich_studio::account_context::{AccountContext, Role};
use rich_studio::errors::StoreError;
use rich_studio::services::accounts;
use rich_studio::store::db::Store;
use rich_studio::store::drafts;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, fake_server, login_as, public_settings, settings,
};

#[tokio::test]
async fn draft_count_limit_serializes_writers_and_allows_cleanup() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let config = public_settings(&dir, &base);
    let app = build_router(AppState::new(config.clone()));
    let who = login_as(&app, 101).await;
    let context = AccountContext::account(&config, 101, Role::Publish);
    Store::connect(&context)?.execute(
        "with recursive seq(n) as (select 1 union all select n+1 from seq where n<998)
         insert into drafts(id,title,payload,created_at,updated_at)
         select 'seed-'||n,'','{}',0,0 from seq",
        [],
    )?;
    let mut tasks = Vec::new();
    for index in 0..8 {
        let app = app.clone();
        let who = who.clone();
        tasks.push(tokio::spawn(async move {
            call(
                app,
                "POST",
                "/api/drafts",
                Some(json!({"title":index.to_string(),"post":blocks_post()})),
                &[("cookie", who.as_str())],
            )
            .await
        }));
    }
    let mut statuses = Vec::new();
    let mut created = Vec::new();
    for task in tasks {
        let reply = task.await?;
        statuses.push(reply.status);
        if reply.status == StatusCode::OK {
            created.push(reply.json()["id"].as_str().unwrap_or_default().to_owned());
        }
    }
    statuses.sort_by_key(|status| status.as_u16());
    let mut expected = vec![StatusCode::OK; 2];
    expected.extend(std::iter::repeat_n(StatusCode::PAYLOAD_TOO_LARGE, 6));
    assert_eq!(statuses, expected);
    let did = created
        .first()
        .unwrap_or_else(|| panic!("no created draft"));
    assert_eq!(
        call(
            app.clone(),
            "PUT",
            &format!("/api/drafts/{did}"),
            Some(json!({"post":blocks_post()})),
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "DELETE",
            &format!("/api/drafts/{did}"),
            None,
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app,
            "POST",
            "/api/drafts",
            Some(json!({"title":"9","post":blocks_post()})),
            &[("cookie", who.as_str())],
        )
        .await
        .status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn public_draft_byte_limit_covers_all_stored_fields() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let app = build_router(AppState::new(public_settings(&dir, &base)));
    let who = login_as(&app, 101).await;
    let huge = "a".repeat(1024 * 1024 + 1);
    for field in ["title", "post", "post_id", "chat_ids"] {
        let value = match field {
            "post" => json!({"rich_message":{"html":huge}}),
            "chat_ids" => json!([format!("@{huge}")]),
            _ => json!(huge),
        };
        let reply = call(
            app.clone(),
            "POST",
            "/api/drafts",
            Some(json!({"post":blocks_post(),field:value})),
            &[("cookie", who.as_str())],
        )
        .await;
        assert_eq!(
            reply.status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "{field}: {}",
            reply.text()
        );
    }
    Ok(())
}

#[tokio::test]
async fn lowered_database_quota_keeps_existing_data_readable_and_deletable() -> TestResult {
    let dir = TempDir::new()?;
    let config = settings(&dir, "http://127.0.0.1:9");
    let context = AccountContext::account(&config, 101, Role::Publish);
    accounts::initialize(&config, 101)?;
    let mut saved = Vec::new();
    for index in 0..8 {
        saved.push(
            drafts::save(
                &context,
                None,
                &index.to_string(),
                &json!({"html":"x".repeat(200_000)}),
                None,
                None,
                None,
            )?
            .unwrap_or_else(|| panic!("draft {index} is missing")),
        );
    }
    let mut strict_config = config.clone();
    strict_config.account_database_mb = 1.0;
    let strict = AccountContext::account(&strict_config, 101, Role::Publish);
    let id = saved[0]["id"].as_str().unwrap_or_default();
    assert_eq!(drafts::get(&strict, id)?, Some(saved[0].clone()));
    let error = drafts::save(
        &strict,
        None,
        "No further growth",
        &json!({"html":"y".repeat(200_000)}),
        None,
        None,
        None,
    )
    .err()
    .unwrap_or_else(|| panic!("growth was accepted past the lowered quota"));
    assert!(matches!(error, StoreError::StorageFull), "{error}");
    drafts::delete(&strict, id)?;
    assert!(
        drafts::save(
            &strict,
            None,
            "Reuses freed pages",
            &json!({"html":"z".repeat(100_000)}),
            None,
            None,
            None,
        )?
        .is_some()
    );
    Ok(())
}
