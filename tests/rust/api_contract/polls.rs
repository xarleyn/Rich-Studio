use axum::http::StatusCode;
use rich_studio::build_router;
use rich_studio::services::updates;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::support::{TestResult, blocks_post, call, merged, state, telegram};

#[tokio::test]
async fn poll_after_follows_the_post_and_inherits_options() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let mut post = blocks_post();
    post["options"] = json!({"disable_notification":true});
    post["poll_after"] = poll.clone();
    let reply = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.value["warnings"], json!([]), "{}", reply.text);
    let methods = telegram
        .calls()
        .into_iter()
        .filter(|call| ["sendRichMessage", "sendPoll"].contains(&call.method.as_str()))
        .map(|call| call.method)
        .collect::<Vec<_>>();
    assert_eq!(methods, ["sendRichMessage", "sendPoll"]);
    assert!(
        telegram
            .last("sendRichMessage")
            .ok_or("send")?
            .body
            .get("poll_after")
            .is_none()
    );
    let sent_poll = telegram.last("sendPoll").ok_or("poll")?.body;
    assert_eq!(sent_poll["question"], "Во сколько эфир?");
    assert_eq!(
        sent_poll["options"],
        json!([{"text":"Утром"},{"text":"Вечером"}])
    );
    assert_eq!(sent_poll["disable_notification"], true);
    assert_eq!(sent_poll["chat_id"], -1001);
    let rows = call(app.clone(), "GET", "/api/posts", None, &[])
        .await
        .value;
    let rows = rows.as_array().ok_or("posts")?;
    assert_eq!(
        rows.iter()
            .take(2)
            .map(|post| post["kind"].clone())
            .collect::<Vec<_>>(),
        [json!("poll"), json!("rich")]
    );
    assert_eq!(rows[0]["status"], "sent");
    let dry = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":post,"dry_run":true})),
        &[],
    )
    .await
    .value;
    assert!(
        dry["after_send"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|step| step == "sendPoll (poll_after)"),
        "{dry}"
    );

    telegram.fail("sendPoll", "Bad Request: poll can't be sent");
    let failed = call(
        app.clone(),
        "POST",
        "/api/send",
        Some(json!({"post":merged(blocks_post(),json!({"poll_after":poll}))})),
        &[],
    )
    .await;
    assert_eq!(failed.status, StatusCode::OK, "{}", failed.text);
    assert_eq!(failed.value["post"]["status"], "sent");
    assert!(
        failed.value["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|warning| warning.contains("опрос после поста")),
        "{}",
        failed.text
    );
    let poll_row = call(app, "GET", "/api/posts?status=failed", None, &[])
        .await
        .value;
    let poll_row = poll_row
        .as_array()
        .into_iter()
        .flatten()
        .find(|post| post["kind"] == "poll")
        .ok_or("failed poll")?;
    assert!(
        poll_row["error"]
            .as_str()
            .unwrap_or_default()
            .contains("poll can't be sent"),
        "{poll_row}"
    );
    Ok(())
}

#[tokio::test]
async fn poll_after_is_validated_like_sendpoll() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let cases = [
        (
            json!({"question":"?","options":["один"]}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "два варианта",
        ),
        (
            merged(poll.clone(), json!({"type":"quiz"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "правильный ответ",
        ),
        (
            merged(poll.clone(), json!({"is_anonymous":"да"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "is_anonymous",
        ),
    ];
    for (bad, status, fragment) in cases {
        let reply = call(
            app.clone(),
            "POST",
            "/api/send",
            Some(json!({"post":merged(blocks_post(),json!({"poll_after":bad}))})),
            &[],
        )
        .await;
        assert_eq!(reply.status, status, "{fragment}: {}", reply.text);
        assert!(reply.text.contains(fragment), "{fragment}: {}", reply.text);
    }
    let thread = call(
        app,
        "POST",
        "/api/send",
        Some(json!({
            "post":merged(blocks_post(),json!({"poll_after":merged(poll.clone(),json!({"message_thread_id":5}))})),
            "chat_ids":["@a","@b"],
            "dry_run":true
        })),
        &[],
    )
    .await;
    assert_eq!(thread.status, StatusCode::BAD_REQUEST, "{}", thread.text);
    assert!(thread.text.contains("тема форума"), "{}", thread.text);
    assert_eq!(telegram.count("sendRichMessage"), 0);
    assert_eq!(telegram.count("sendPoll"), 0);
    Ok(())
}

#[tokio::test]
async fn poll_validation_send_results_and_stop() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let state = state(&dir, &base, &[]);
    let app = build_router(state.clone());
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/poll",
            Some(json!({"poll":{"question":"?","options":["один"]}})),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            app.clone(),
            "POST",
            "/api/poll",
            Some(json!({"poll":{"question":"?","options":["a","b"],"chat_id":"@x"}})),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert!(telegram.calls().is_empty());

    let poll = json!({
        "question":"Когда эфир?",
        "options":["Утром",{"text":"Вечером"}],
        "members_only":true,
        "country_codes":["RU","FT"],
        "description":"Голосуют подписчики старше суток"
    });
    let reply = call(
        app.clone(),
        "POST",
        "/api/poll",
        Some(json!({"poll":poll,"pin":true})),
        &[],
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = telegram.last("sendPoll").ok_or("sendPoll")?.body;
    assert_eq!(
        body["options"],
        json!([{"text":"Утром"},{"text":"Вечером"}])
    );
    assert_eq!(body["members_only"], true);
    assert_eq!(body["country_codes"], json!(["RU", "FT"]));
    assert_eq!(body["chat_id"], "@test_channel");
    assert!(body.get("pin").is_none(), "{body}");
    assert!(telegram.last("pinChatMessage").is_some());
    let record = reply.value["results"][0]["post"].clone();
    assert_eq!(record["kind"], "poll");
    let id = record["id"].as_str().ok_or("poll id")?.to_owned();
    let poll_id = record["poll_id"].as_str().ok_or("poll_id")?.to_owned();
    updates::handle_update(
        &state,
        &json!({
            "update_id":5,
            "poll":{
                "id":poll_id,
                "total_voter_count":4,
                "is_closed":false,
                "options":[
                    {"text":"Утром","voter_count":3},
                    {"text":"Вечером","voter_count":1}
                ]
            }
        }),
    )
    .await?;
    assert_eq!(
        call(app.clone(), "GET", &format!("/api/posts/{id}"), None, &[])
            .await
            .value["poll_results"]["total"],
        4
    );
    let stopped = call(
        app.clone(),
        "POST",
        &format!("/api/posts/{id}/stop-poll"),
        None,
        &[],
    )
    .await;
    assert_eq!(stopped.value["poll_results"]["is_closed"], true);
    assert_eq!(
        call(
            app,
            "POST",
            &format!("/api/posts/{id}/edit"),
            Some(blocks_post()),
            &[],
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn poll_that_may_have_gone_stays_listed() -> TestResult {
    let dir = TempDir::new()?;
    let (base, telegram) = telegram().await;
    let app = build_router(state(&dir, &base, &[]));
    let poll = json!({"question":"Во сколько эфир?","options":["Утром",{"text":"Вечером"}]});
    let draft = call(
        app.clone(),
        "POST",
        "/api/drafts",
        Some(json!({"title":"С опросом","post":blocks_post()})),
        &[],
    )
    .await
    .value;
    let draft_id = draft["id"].as_str().ok_or("draft id")?.to_owned();
    telegram.drop_responses("sendPoll", 2);
    for _ in 0..2 {
        assert_eq!(
            call(
                app.clone(),
                "POST",
                "/api/send",
                Some(json!({"post":merged(blocks_post(),json!({"poll_after":poll})),"draft_id":draft_id})),
                &[],
            )
            .await
            .status,
            StatusCode::OK
        );
    }
    let gone = call(app, "GET", "/api/posts?status=failed", None, &[])
        .await
        .value;
    let gone = gone
        .as_array()
        .into_iter()
        .flatten()
        .filter(|post| post["kind"] == "poll")
        .collect::<Vec<_>>();
    assert_eq!(gone.len(), 2, "{gone:?}");
    assert!(
        gone.iter().all(|post| post["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Опрос мог уйти")),
        "{gone:?}"
    );
    Ok(())
}
