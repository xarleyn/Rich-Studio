use axum::{Json, Router, extract::State, routing::post};
use rich_studio::{
    AppState,
    account_context::AccountContext,
    services::{health, in_flight, loops, scheduler},
    settings::Settings,
    store::{
        db::{self, Store},
        meta, posts,
    },
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;

#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    mode: u8,
    accepted: Arc<Notify>,
    release: Arc<Notify>,
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn reply(
    State(state): State<Mock>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    state.calls.fetch_add(1, Ordering::SeqCst);
    // Mode 2 holds every call; mode 4 holds only a post send, so background
    // housekeeping (e.g. setMyCommands) still answers while the send is stuck.
    if state.mode == 2 || (state.mode == 4 && uri.path().ends_with("/sendRichMessage")) {
        state.accepted.notify_one();
        state.release.notified().await;
    }
    let status = if state.mode == 1 {
        429
    } else if state.mode == 3 {
        502
    } else {
        200
    };
    let result = match state.mode {
        1 => {
            json!({"ok":false,"error_code":429,"description":"wait","parameters":{"retry_after":120}})
        }
        3 => json!({"ok":false,"description":"lost upstream"}),
        _ => json!({"ok":true,"result":{"message_id":42,"chat":{"id":body["chat_id"]}}}),
    };
    (
        axum::http::StatusCode::from_u16(status).unwrap_or_else(|error| panic!("status: {error}")),
        Json(result),
    )
}
async fn mock(mode: u8) -> (String, Mock, Server) {
    let state = Mock {
        calls: Arc::new(AtomicUsize::new(0)),
        mode,
        accepted: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("listen: {error}"));
    let base = format!(
        "http://{}",
        listener
            .local_addr()
            .unwrap_or_else(|error| panic!("address: {error}"))
    );
    let app = Router::new()
        .route("/{*path}", post(reply))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .unwrap_or_else(|error| panic!("mock: {error}"));
    });
    (base, state, Server(task))
}
fn app(dir: &TempDir, base: &str) -> AppState {
    app_with(dir, base, &[])
}
fn app_with(dir: &TempDir, base: &str, extra: &[(&str, &str)]) -> AppState {
    let mut values = BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            dir.path().to_string_lossy().into_owned(),
        ),
        ("TG_API_BASE".to_owned(), base.to_owned()),
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        ("STUDIO_BACKGROUND".to_owned(), "off".to_owned()),
        ("SCHEDULE_LATE_MIN".to_owned(), "60".to_owned()),
    ]);
    for (key, value) in extra {
        values.insert((*key).to_owned(), (*value).to_owned());
    }
    let settings = Settings::from_map(&values).unwrap_or_else(|error| panic!("settings: {error}"));
    AppState::new(settings)
}
fn background_app(dir: &TempDir) -> AppState {
    app_with(
        dir,
        "http://127.0.0.1:9",
        &[
            ("STUDIO_BACKGROUND", "on"),
            ("TG_UPDATES", "off"),
            ("SPEC_PIN", "on"),
            ("BACKUP_KEEP", "0"),
        ],
    )
}
fn add(context: &AccountContext, at: i64) -> String {
    posts::add_scheduled(
        context,
        &[json!(-100123)],
        &json!({"rich_message":{"markdown":"scheduled"}}),
        at,
        None,
        "rich",
        "api",
    )
    .unwrap_or_else(|error| panic!("scheduled row: {error}"))[0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("post id"))
        .to_owned()
}
fn stored(context: &AccountContext, status: &str) -> String {
    let chat = json!(-100123);
    let payload = json!({"rich_message":{"markdown":"scheduled"}});
    posts::add(
        context,
        posts::NewPost {
            chat_id: &chat,
            payload: &payload,
            draft_id: None,
            status,
            message_id: None,
            scheduled_at: None,
            error: None,
            kind: "rich",
            actor: "api",
        },
    )
    .unwrap_or_else(|error| panic!("post row: {error}"))["id"]
        .as_str()
        .unwrap_or_else(|| panic!("post id"))
        .to_owned()
}
fn post_status(context: &AccountContext, id: &str) -> String {
    posts::get(context, id)
        .unwrap_or_else(|error| panic!("post: {error}"))
        .unwrap_or_else(|| panic!("post {id} is missing"))["status"]
        .as_str()
        .unwrap_or_else(|| panic!("post status"))
        .to_owned()
}
async fn wait_post_status(context: &AccountContext, id: &str, status: &str, seconds: u64) {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        loop {
            if post_status(context, id) == status {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("post {id} did not become {status}"));
}
async fn wait_role(state: &AppState, role: &str, seconds: u64) {
    tokio::time::timeout(Duration::from_secs(seconds), async {
        loop {
            if state.loop_status.read().await.role == role {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("loops did not become {role}"));
}
fn break_lease(context: &AccountContext) {
    let mut connection = Store::connect(context).unwrap_or_else(|error| panic!("store: {error}"));
    db::migrate(&mut connection).unwrap_or_else(|error| panic!("migrate: {error}"));
    connection
        .execute_batch(
            "delete from meta where key='lease:loops';\n\
             create trigger broken_lease_insert before insert on meta when new.key='lease:loops' \
             begin select raise(abort, 'database or disk is full'); end;\n\
             create trigger broken_lease_update before update on meta when new.key='lease:loops' \
             begin select raise(abort, 'database or disk is full'); end;",
        )
        .unwrap_or_else(|error| panic!("break lease: {error}"));
}
fn repair_lease(context: &AccountContext) {
    Store::connect(context)
        .unwrap_or_else(|error| panic!("store: {error}"))
        .execute_batch("drop trigger broken_lease_insert; drop trigger broken_lease_update;")
        .unwrap_or_else(|error| panic!("repair lease: {error}"));
}
#[tokio::test]
async fn due_claim_and_recovery_never_repeat_confirmed_or_unknown_sends() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(0).await;
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    let stop = Arc::new(AtomicBool::new(false));
    assert_eq!(
        scheduler::run_due(&state, state.account.clone(), 1001, stop.clone())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "sent"
    );
    assert_eq!(
        scheduler::run_due(&state, state.account.clone(), 1002, stop)
            .await
            .unwrap(),
        0
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    let interrupted = add(&state.account, 1000);
    assert!(rich_studio::store::scheduler::claim(&state.account, &interrupted, 1001).unwrap());
    assert_eq!(in_flight::recover(&state.account).unwrap(), 1);
    assert_eq!(
        posts::get(&state.account, &interrupted).unwrap().unwrap()["status"],
        "failed"
    );
}

#[test]
fn stale_due_snapshot_cannot_send_or_mark_missed_a_rescheduled_post() {
    let dir = TempDir::new().unwrap();
    let state = app(&dir, "http://127.0.0.1:1");
    let id = add(&state.account, 1000);
    assert_eq!(
        rich_studio::store::scheduler::due(&state.account, 5000)
            .unwrap()
            .len(),
        1
    );
    assert!(rich_studio::store::editing::reschedule(&state.account, &id, 9000).unwrap());
    assert!(
        in_flight::claim(&state.account, &id, 5000)
            .unwrap()
            .is_none()
    );
    assert!(!rich_studio::store::scheduler::missed(&state.account, &id, "stale snapshot").unwrap());
    let row = posts::get(&state.account, &id).unwrap().unwrap();
    assert_eq!(row["status"], "scheduled");
    assert_eq!(row["scheduled_at"], 9000);
    let claim = in_flight::claim(&state.account, &id, 9001)
        .unwrap()
        .unwrap();
    assert!(!rich_studio::store::editing::reschedule(&state.account, &id, 10000).unwrap());
    drop(claim);
}
#[tokio::test]
async fn rate_limit_is_requeued_but_ambiguous_gateway_is_never_retried() {
    for (mode, status, retry) in [(1, "scheduled", Some(1121)), (3, "failed", None)] {
        let dir = TempDir::new().unwrap();
        let (base, mock, _server) = mock(mode).await;
        let state = app(&dir, &base);
        let id = add(&state.account, 1000);
        scheduler::run_due(
            &state,
            state.account.clone(),
            1001,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let row = posts::get(&state.account, &id).unwrap().unwrap();
        assert_eq!(row["status"], status);
        assert_eq!(row["retry_at"].as_i64(), retry);
        scheduler::run_due(
            &state,
            state.account.clone(),
            1002,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn stopped_and_late_queues_do_not_publish() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(0).await;
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            1001,
            Arc::new(AtomicBool::new(true))
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "scheduled"
    );
    scheduler::run_due(
        &state,
        state.account.clone(),
        5000,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "missed"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn cancellation_after_accept_leaves_a_marker_and_live_sends_are_excluded_from_recovery() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(2).await;
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    let cloned = state.clone();
    let task = tokio::spawn(async move {
        scheduler::run_due(
            &cloned,
            cloned.account.clone(),
            1001,
            Arc::new(AtomicBool::new(false)),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), mock.accepted.notified())
        .await
        .unwrap();
    assert_eq!(in_flight::recover(&state.account).unwrap(), 0);
    assert_eq!(
        posts::get(&state.account, &id).unwrap().unwrap()["status"],
        "sending"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!in_flight::contains(&state.account, &id));
    assert_eq!(in_flight::recover(&state.account).unwrap(), 1);
    scheduler::run_due(
        &state,
        state.account.clone(),
        1002,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert!(
        posts::get(&state.account, &id).unwrap().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("мог уйти")
    );
}
#[test]
fn leases_and_webhook_claims_are_atomic_and_survive_reopen() {
    let dir = TempDir::new().unwrap();
    let state = app(&dir, "http://127.0.0.1:9");
    let context = state.account;
    assert!(meta::take_lease(&context, "loops", "one", 60.0, 1000.0).unwrap());
    assert!(!meta::take_lease(&context, "loops", "two", 60.0, 1001.0).unwrap());
    assert!(meta::take_lease(&context, "loops", "one", 60.0, 1050.0).unwrap());
    assert_eq!(
        meta::lease_holder(&context, "loops").unwrap().as_deref(),
        Some("one")
    );
    assert!(!meta::take_lease(&context, "loops", "two", 60.0, 1061.0).unwrap());
    assert!(meta::take_lease(&context, "loops", "two", 60.0, 1111.0).unwrap());
    meta::drop_lease(&context, "loops", "one").unwrap();
    assert_eq!(
        meta::lease_holder(&context, "loops").unwrap().as_deref(),
        Some("two")
    );
    meta::drop_lease(&context, "loops", "two").unwrap();
    assert_eq!(meta::lease_holder(&context, "loops").unwrap(), None);
    assert!(meta::claim_webhook_update(&context, 77, 1000).unwrap());
    assert!(!meta::claim_webhook_update(&context, 77, 1001).unwrap());
    assert!(loops::may_keep_leading(
        true,
        std::time::Duration::from_secs(39)
    ));
    assert!(!loops::may_keep_leading(
        true,
        std::time::Duration::from_secs(40)
    ));
    assert!(!loops::may_keep_leading(false, std::time::Duration::ZERO));
    let mut connection = Store::connect(&context).unwrap();
    db::migrate(&mut connection).unwrap();
}
#[test]
fn startup_recovery_keeps_what_this_process_is_sending() {
    let dir = TempDir::new().unwrap();
    let state = app(&dir, "http://127.0.0.1:9");
    let context = state.account.clone();
    let mine = stored(&context, "sending");
    let left = stored(&context, "sending");
    let record = posts::get(&context, &mine).unwrap().unwrap();
    let (_record, guard) = in_flight::record(&context, || Ok(record)).unwrap();
    assert_eq!(in_flight::recover(&context).unwrap(), 1);
    assert_eq!(post_status(&context, &mine), "sending");
    assert_eq!(post_status(&context, &left), "failed");
    drop(guard);
    assert!(!in_flight::contains(&context, &mine));
}
#[test]
fn database_uses_wal() {
    let dir = TempDir::new().unwrap();
    let state = app(&dir, "http://127.0.0.1:9");
    let mut connection = Store::connect(&state.account).unwrap();
    db::migrate(&mut connection).unwrap();
    let mode: String = connection
        .query_row("pragma journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}
#[tokio::test]
async fn standby_while_another_process_leads_then_takes_over() {
    let dir = TempDir::new().unwrap();
    let state = background_app(&dir);
    let context = state.account.clone();
    assert!(meta::take_lease(&context, "loops", "other", 60.0, scheduler::now() as f64).unwrap());
    let left = stored(&context, "sending");
    let runner = loops::Loops::start(state.clone());
    wait_role(&state, "standby", 5).await;
    {
        let status = state.loop_status.read().await;
        assert_eq!(status.role, "standby");
        assert_eq!(status.holder.as_deref(), Some("other"));
    }
    // A standby never touches another process's sends.
    assert_eq!(post_status(&context, &left), "sending");
    let report = {
        let spec = state.spec.read().await;
        let loop_status = state.loop_status.read().await;
        let updates_status = state.updates_status.read().await;
        health::report(
            &state.settings,
            &state.account,
            &spec,
            &state.logs,
            &state.metrics,
            &loop_status,
            &updates_status,
        )
    };
    let loops_check = report["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("checks"))
        .iter()
        .find(|check| check["name"] == "loops")
        .unwrap_or_else(|| panic!("loops check"));
    assert_eq!(loops_check["status"], "warn");
    // The other process stopped renewing: its lease is over before this process
    // starts a keeper again, and the cut-off send is recovered as failed.
    assert!(meta::take_lease(&context, "loops", "other", 60.0, 0.0).unwrap());
    drop(runner);
    let runner = loops::Loops::start(state.clone());
    wait_role(&state, "leader", 10).await;
    assert_eq!(post_status(&context, &left), "failed");
    {
        let status = state.loop_status.read().await;
        assert_eq!(status.role, "leader");
        assert_eq!(status.holder.as_deref(), Some(status.owner.as_str()));
    }
    runner.stop().await.unwrap();
    assert_eq!(state.loop_status.read().await.role, "off");
    assert!(meta::lease_holder(&context, "loops").unwrap().is_none());
}
#[tokio::test]
async fn stop_waits_for_sends_under_way_and_takes_no_new_posts() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(4).await;
    let state = app_with(
        &dir,
        &base,
        &[
            ("STUDIO_BACKGROUND", "on"),
            ("TG_UPDATES", "off"),
            ("SPEC_PIN", "on"),
            ("BACKUP_KEEP", "0"),
            ("SHUTDOWN_GRACE", "5"),
        ],
    );
    let context = state.account.clone();
    let id = add(&context, scheduler::now() - 5);
    let runner = loops::Loops::start(state.clone());
    wait_post_status(&context, &id, "sending", 5).await;
    let release = mock.release.clone();
    let finisher = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        release.notify_waiters();
    });
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(10), runner.stop())
        .await
        .unwrap_or_else(|_| panic!("stop did not return"))
        .unwrap_or_else(|error| panic!("stop: {error}"));
    let waited = started.elapsed();
    finisher.await.unwrap();
    assert!(
        waited >= Duration::from_millis(250),
        "stop cut the send short instead of waiting for it: {waited:?}"
    );
    assert_eq!(post_status(&context, &id), "sent");
    assert!(state.loop_status.read().await.stopping);
    let calls = mock.calls.load(Ordering::SeqCst);
    // Shutting down takes no new posts: this one waits for the next start.
    let due = add(&context, scheduler::now() - 5);
    assert_eq!(
        scheduler::run_due(
            &state,
            context.clone(),
            scheduler::now(),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(post_status(&context, &due), "scheduled");
    assert_eq!(mock.calls.load(Ordering::SeqCst), calls);
}
#[tokio::test]
async fn database_outage_hands_the_loops_back_once_it_is_over() {
    let dir = TempDir::new().unwrap();
    let state = background_app(&dir);
    let context = state.account.clone();
    let runner = loops::Loops::start(state.clone());
    wait_role(&state, "leader", 10).await;
    let stale = stored(&context, "sending");
    break_lease(&context);
    // One failed renewal: the lease we took is surely still running, keep leading.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if state.loop_status.read().await.error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("lease renewal did not fail"));
    assert_eq!(state.loop_status.read().await.role, "leader");
    // The second failed renewal is LEASE_TTL - RENEW seconds after the last one:
    // another process may take over soon, so stop the loops and stand by.
    wait_role(&state, "standby", 30).await;
    {
        let status = state.loop_status.read().await;
        assert_eq!(status.role, "standby");
        assert_eq!(status.holder, None);
    }
    assert_eq!(post_status(&context, &stale), "sending");
    repair_lease(&context);
    // The same keeper is still alive: its next round leads again and recovers
    // the send that was cut off while nobody could renew the lease.
    wait_role(&state, "leader", 30).await;
    assert_eq!(post_status(&context, &stale), "failed");
    runner.stop().await.unwrap();
    assert_eq!(state.loop_status.read().await.role, "off");
    assert!(meta::lease_holder(&context, "loops").unwrap().is_none());
}
#[tokio::test]
async fn no_connection_is_definite_and_safe_to_retry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let dir = TempDir::new().unwrap();
    let state = app(&dir, &base);
    let id = add(&state.account, 1000);
    scheduler::run_due(
        &state,
        state.account.clone(),
        1001,
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .unwrap();
    let row = posts::get(&state.account, &id).unwrap().unwrap();
    assert_eq!(row["status"], "scheduled");
    assert_eq!(row["retry_at"], 1061);
}

#[tokio::test]
async fn unknown_kind_fails_without_calling_telegram() {
    let dir = TempDir::new().unwrap();
    let (base, mock, _server) = mock(0).await;
    let state = app(&dir, &base);
    let records = posts::add_scheduled(
        &state.account,
        &[json!(-100123)],
        &json!({"rich_message":{"markdown":"scheduled"}}),
        1000,
        None,
        "story",
        "api",
    )
    .unwrap_or_else(|error| panic!("scheduled row: {error}"));
    let id = records[0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("post id"));
    assert_eq!(
        scheduler::run_due(
            &state,
            state.account.clone(),
            1001,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap(),
        1
    );
    let row = posts::get(&state.account, id).unwrap().unwrap();
    assert_eq!(row["status"], "failed");
    assert!(row["error"].as_str().unwrap_or_default().contains("story"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
}
