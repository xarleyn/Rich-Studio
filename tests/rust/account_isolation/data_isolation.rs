use axum::http::StatusCode;
use rich_studio::account_context::AccountContext;
use rich_studio::store::db::{self, Store};
use rich_studio::store::drafts;
use rich_studio::{AppState, build_router};
use serde_json::json;
use tempfile::TempDir;

use crate::support::{
    TestResult, blocks_post, call, connect_channel, fake_server, login_as, ok_envelope,
    public_config, upload,
};

#[tokio::test]
async fn accounts_and_operator_have_separate_drafts_and_files() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let root = AccountContext::root(&settings);
    let mut root_db = Store::connect(&root)?;
    db::migrate(&mut root_db)?;
    drop(root_db);
    drafts::save(
        &root,
        None,
        "Личный черновик владельца",
        &blocks_post(),
        None,
        None,
        None,
    )?;
    let app = build_router(AppState::new(settings));

    let first = login_as(&app, 101).await;
    let created = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"Первый","post":blocks_post()})),
        &[("cookie", &first)],
    )
    .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.text());
    let created = created.json();
    let id = created["id"].as_str().expect("draft id").to_owned();
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/drafts",
            None,
            &[("cookie", &first)]
        )
        .await
        .json()
        .as_array()
        .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        upload(app.clone(), &first, "mine.txt", b"first")
            .await
            .status,
        StatusCode::OK
    );

    let second = login_as(&app, 202).await;
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/drafts",
            None,
            &[("cookie", &second)]
        )
        .await
        .json(),
        json!([])
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/api/media",
            None,
            &[("cookie", &second)]
        )
        .await
        .json(),
        json!([])
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &format!("/api/drafts/{id}"),
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            &format!("/render/draft/{id}"),
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/media/mine.txt",
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        upload(app.clone(), &second, "mine.txt", b"second")
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/media/mine.txt",
            None,
            &[("cookie", &first)]
        )
        .await
        .bytes
        .as_slice(),
        b"first"
    );
    assert_eq!(
        call(
            app.clone(),
            "GET",
            "/media/mine.txt",
            None,
            &[("cookie", &second)]
        )
        .await
        .bytes
        .as_slice(),
        b"second"
    );
    assert_eq!(
        call(
            app,
            "GET",
            "/media/..%2F..%2Fstudio.db",
            None,
            &[("cookie", &second)]
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let titles: Vec<String> = drafts::list(&root)?
        .into_iter()
        .filter_map(|draft| draft["title"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(titles, vec!["Личный черновик владельца".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn parallel_requests_keep_their_account_context() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings.clone()));

    let first = login_as(&app, 101).await;
    let second = login_as(&app, 202).await;
    let mut tasks = Vec::new();
    for index in 0..12 {
        let app = app.clone();
        let who = if index % 2 == 1 {
            first.clone()
        } else {
            second.clone()
        };
        let title = (index % 2).to_string();
        tasks.push(tokio::spawn(async move {
            let reply = call(
                app,
                "POST",
                "/api/drafts",
                Some(json!({"title":title,"post":blocks_post()})),
                &[("cookie", &who)],
            )
            .await;
            (reply.status, reply.json())
        }));
    }
    for task in tasks {
        let (status, body) = task.await?;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let first_reply = call(
        app.clone(),
        "GET",
        "/api/drafts",
        None,
        &[("cookie", &first)],
    )
    .await
    .json();
    let first_titles: Vec<String> = first_reply
        .as_array()
        .expect("drafts")
        .iter()
        .filter_map(|draft| draft["title"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(first_titles, vec!["1".to_owned(); 6]);
    let second_reply = call(app, "GET", "/api/drafts", None, &[("cookie", &second)])
        .await
        .json();
    let second_titles: Vec<String> = second_reply
        .as_array()
        .expect("drafts")
        .iter()
        .filter_map(|draft| draft["title"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(second_titles, vec!["0".to_owned(); 6]);
    assert!(drafts::list(&AccountContext::root(&settings))?.is_empty());
    Ok(())
}

#[tokio::test]
async fn quota_removes_partial_upload_and_other_accounts_have_their_own_quota() -> TestResult {
    let dir = TempDir::new()?;
    let (base, _fake) = fake_server().await;
    let mut settings = public_config(&dir, &base);
    settings.account_storage_mb = 4.0 / (1024.0 * 1024.0);
    let app = build_router(AppState::new(settings));

    let first = login_as(&app, 101).await;
    assert_eq!(
        upload(app.clone(), &first, "one.txt", b"1234").await.status,
        StatusCode::OK
    );
    assert_eq!(
        upload(app.clone(), &first, "two.txt", b"x").await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let names: Vec<String> = call(
        app.clone(),
        "GET",
        "/api/media",
        None,
        &[("cookie", &first)],
    )
    .await
    .json()
    .as_array()
    .expect("media")
    .iter()
    .filter_map(|file| file["name"].as_str().map(str::to_owned))
    .collect();
    assert_eq!(names, vec!["one.txt".to_owned()]);

    let second = login_as(&app, 202).await;
    assert_eq!(
        upload(app, &second, "two.txt", b"1234").await.status,
        StatusCode::OK
    );
    Ok(())
}

#[tokio::test]
async fn username_and_numeric_id_of_one_channel_do_not_send_twice() -> TestResult {
    let dir = TempDir::new()?;
    let (base, fake) = fake_server().await;
    let settings = public_config(&dir, &base);
    let app = build_router(AppState::new(settings));

    let who = login_as(&app, 101).await;
    fake.queue(
        "getChat",
        vec![ok_envelope(json!({
            "id":-1001,"type":"channel","title":"Мой канал","username":"mine"
        }))],
    );
    connect_channel(&app, &who).await;
    let reply = call(
        app,
        "POST",
        "/api/send",
        Some(json!({"post":blocks_post(),"chat_ids":["@mine","-1001"]})),
        &[("cookie", &who)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["results"].as_array().map(Vec::len), Some(1));
    assert_eq!(fake.count("sendRichMessage"), 1);
    Ok(())
}
