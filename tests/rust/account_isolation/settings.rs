use std::collections::BTreeMap;

use rich_studio::account_context::{AccountContext, Role, TransportPolicy};
use rich_studio::errors::StoreError;
use rich_studio::settings::Settings;
use rich_studio::store::db::{self, Store};
use tempfile::TempDir;

use crate::support::config;

#[test]
fn every_bad_setting_is_named_at_once() {
    let error = Settings::from_map(&BTreeMap::from([
        ("LOG_FILES".to_owned(), "x".to_owned()),
        ("SESSION_DAYS".to_owned(), "-1".to_owned()),
        ("LOG_LEVEL".to_owned(), "loud".to_owned()),
        ("STUDIO_MODE".to_owned(), "prod".to_owned()),
    ]))
    .expect_err("invalid settings");
    let names: Vec<&str> = error
        .problems
        .iter()
        .map(|problem| problem.split('=').next().expect("setting name"))
        .collect();
    assert_eq!(
        names,
        ["STUDIO_MODE", "SESSION_DAYS", "LOG_LEVEL", "LOG_FILES"]
    );
}

#[test]
fn secrets_come_from_files_and_ambiguous_secrets_are_rejected() {
    let dir = TempDir::new().expect("temp dir");
    let token = dir.path().join("token");
    std::fs::write(&token, "123:FROM_FILE\n").expect("secret fixture");
    let config = config(
        dir.path(),
        &[("TG_BOT_TOKEN_FILE", token.to_str().expect("utf-8 path"))],
    );
    assert_eq!(config.bot_token, "123:FROM_FILE");

    let error = Settings::from_map(&BTreeMap::from([(
        "STUDIO_API_KEY_FILE".to_owned(),
        "/definitely/missing/studio-key".to_owned(),
    )]))
    .expect_err("missing secret file");
    assert!(error.problems[0].starts_with("STUDIO_API_KEY_FILE"));

    let error = Settings::from_map(&BTreeMap::from([
        ("TG_BOT_TOKEN".to_owned(), "1:x".to_owned()),
        (
            "TG_BOT_TOKEN_FILE".to_owned(),
            token.to_string_lossy().into_owned(),
        ),
    ]))
    .expect_err("ambiguous secret");
    assert!(error.problems[0].contains("оставьте одно"));
}

#[test]
fn account_paths_are_explicit_and_never_fall_back_to_owner() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(dir.path(), &[]);
    let root = AccountContext::root(&config);
    let first = AccountContext::account(&config, 101, Role::Publish);
    let second = AccountContext::account(&config, 202, Role::Drafts);

    assert_ne!(first.database_path(), root.database_path());
    assert_ne!(first.database_path(), second.database_path());
    assert_eq!(first.media_dir, dir.path().join("accounts/101/media"));
    assert_eq!(second.media_dir, dir.path().join("accounts/202/media"));
    assert_eq!(first.transport_policy, TransportPolicy::Account(101));
    assert_eq!(second.transport_policy, TransportPolicy::Account(202));
    assert_eq!(root.transport_policy, TransportPolicy::Owner);

    let mut first_db = Store::connect(&first).expect("first account database");
    let mut second_db = Store::connect(&second).expect("second account database");
    let mut root_db = Store::connect(&root).expect("owner database");
    for connection in [&mut first_db, &mut second_db, &mut root_db] {
        db::migrate(connection).expect("migrations");
    }
    first_db
        .execute(
            "insert into drafts(id, title, payload) values ('mine', 'First', '{}')",
            [],
        )
        .expect("first draft");
    let count = |connection: &rusqlite::Connection| {
        connection
            .query_row("select count(*) from drafts", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("draft count")
    };
    assert_eq!(count(&first_db), 1);
    assert_eq!(count(&second_db), 0);
    assert_eq!(count(&root_db), 0);
}

#[test]
fn account_database_quota_is_enforced_but_root_has_no_account_quota() {
    let dir = TempDir::new().expect("temp dir");
    let config = config(dir.path(), &[("ACCOUNT_DATABASE_MB", "1")]);
    let account = AccountContext::account(&config, 101, Role::Publish);
    let root = AccountContext::root(&config);
    let mut account_db = Store::connect(&account).expect("account connection");
    let root_db = Store::connect(&root).expect("root connection");
    db::migrate(&mut account_db).expect("account migrations");

    let page_size: i64 = account_db
        .query_row("pragma page_size", [], |row| row.get(0))
        .expect("page size");
    let account_pages: i64 = account_db
        .query_row("pragma max_page_count", [], |row| row.get(0))
        .expect("account max pages");
    let root_pages: i64 = root_db
        .query_row("pragma max_page_count", [], |row| row.get(0))
        .expect("root max pages");
    assert_eq!(account_pages, 1024 * 1024 / page_size);
    assert!(root_pages > account_pages);

    let error = account_db
        .execute(
            "insert into drafts(id, title, payload) values ('big', 'Big', zeroblob(2097152))",
            [],
        )
        .expect_err("account quota");
    assert!(matches!(
        StoreError::from_sqlite(error, &account),
        StoreError::StorageFull
    ));
}
