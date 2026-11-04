use std::collections::BTreeMap;
use std::path::Path;

use rich_studio::account_context::AccountContext;
use rich_studio::settings::Settings;
use rich_studio::store::db::{self, Store};
use rusqlite::Connection;
use tempfile::TempDir;

fn settings(data_dir: &Path) -> Settings {
    Settings::from_map(&BTreeMap::from([(
        "DATA_DIR".to_owned(),
        data_dir.to_string_lossy().into_owned(),
    )]))
    .unwrap_or_else(|error| panic!("test settings: {error}"))
}

fn root_connection(data_dir: &Path) -> Connection {
    let config = settings(data_dir);
    Store::connect_root(&config).unwrap_or_else(|error| panic!("root connection: {error}"))
}

#[test]
fn migration_files_are_numbered_one_by_one() {
    let migrations = db::load_migrations(db::MIGRATIONS_DIR).expect("bundled migrations");
    assert_eq!(migrations.len(), 17);
    assert!(migrations.iter().all(|sql| sql.starts_with("-- ")));
}

#[test]
fn duplicate_or_skipped_migration_numbers_stop_startup() {
    let dir = TempDir::new().expect("temp dir");
    for name in ["0001_a.sql", "0002_b.sql", "0002_c.sql"] {
        std::fs::write(dir.path().join(name), "-- test\nselect 1;").expect("migration fixture");
    }
    assert!(db::load_migrations(dir.path()).is_err());
    std::fs::rename(dir.path().join("0002_c.sql"), dir.path().join("0004_c.sql"))
        .expect("renumber fixture");
    assert!(db::load_migrations(dir.path()).is_err());
}

#[test]
fn fresh_database_runs_every_step_with_durable_pragmas() {
    let dir = TempDir::new().expect("temp dir");
    let mut connection = root_connection(dir.path());
    db::migrate(&mut connection).expect("migrate fresh database");

    let version: i64 = connection
        .query_row("pragma user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 17);
    let journal_mode: String = connection
        .query_row("pragma journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(journal_mode.to_lowercase(), "wal");
    for (pragma, expected) in [
        ("foreign_keys", 1),
        ("busy_timeout", 10000),
        ("synchronous", 2),
    ] {
        let actual: i64 = connection
            .query_row(&format!("pragma {pragma}"), [], |row| row.get(0))
            .expect("connection pragma");
        assert_eq!(actual, expected, "pragma {pragma}");
    }
    for table in ["meta", "idempotency", "post_versions", "channels"] {
        let found: i64 = connection
            .query_row(
                "select count(*) from sqlite_master where type='table' and name=?",
                [table],
                |row| row.get(0),
            )
            .expect("table lookup");
        assert_eq!(found, 1, "missing {table}");
    }
}

#[test]
fn old_database_gets_only_new_steps_and_keeps_data() {
    let dir = TempDir::new().expect("temp dir");
    let mut connection = root_connection(dir.path());
    connection
        .execute_batch(db::SCHEMA)
        .expect("old base schema");
    for (column, declaration) in db::EXTRA_COLUMNS {
        connection
            .execute_batch(&format!(
                "alter table posts add column {column} {declaration}"
            ))
            .expect("old extra column");
    }
    let migrations = db::load_migrations(db::MIGRATIONS_DIR).expect("bundled migrations");
    for step in migrations.iter().take(4) {
        connection.execute_batch(step).expect("old migration");
    }
    connection
        .execute_batch(
            "pragma user_version = 4; insert into meta(key, value) values ('kept', '1');",
        )
        .expect("old data");

    db::migrate(&mut connection).expect("upgrade old database");
    assert_eq!(
        connection
            .query_row("select value from meta where key='kept'", [], |row| row
                .get::<_, String>(
                0
            ))
            .expect("kept value"),
        "1"
    );
    let version: i64 = connection
        .query_row("pragma user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 17);
    connection
        .execute_batch("select post_id, chat_ids from drafts;")
        .expect("new draft columns");
}

#[test]
fn failed_step_rolls_back_schema_and_version_then_can_run_again() {
    let dir = TempDir::new().expect("temp dir");
    let mut connection = root_connection(dir.path());
    db::migrate(&mut connection).expect("base migrations");
    let mut migrations = db::load_migrations(db::MIGRATIONS_DIR).expect("bundled migrations");
    let broken = "-- test\ncreate table half_done(x);\ninsert into no_such_table values (1);";
    migrations.push(broken.to_owned());

    assert!(db::migrate_steps(&mut connection, &migrations).is_err());
    let version: i64 = connection
        .query_row("pragma user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 17);
    let half_done: i64 = connection
        .query_row(
            "select count(*) from sqlite_master where name='half_done'",
            [],
            |row| row.get(0),
        )
        .expect("rollback lookup");
    assert_eq!(half_done, 0);

    *migrations.last_mut().expect("broken step") = broken.replace("no_such_table", "half_done");
    db::migrate_steps(&mut connection, &migrations).expect("fixed migration");
    let inserted: i64 = connection
        .query_row("select x from half_done", [], |row| row.get(0))
        .expect("fixed migration result");
    assert_eq!(inserted, 1);
}

#[test]
fn malformed_json_from_an_old_row_is_not_silently_replaced() {
    let dir = TempDir::new().expect("temp dir");
    let mut connection = root_connection(dir.path());
    db::migrate(&mut connection).expect("migrations");
    connection
        .execute(
            "insert into drafts(id, title, payload) values ('old', 'Old', '{not-json')",
            [],
        )
        .expect("old malformed row");
    connection
        .query_row("select payload from drafts where id='old'", [], |row| {
            db::decode_row(row)
        })
        .expect_err("malformed payload must be reported");
}

#[test]
fn root_connection_does_not_require_an_account_context() {
    let dir = TempDir::new().expect("temp dir");
    let config = settings(dir.path());
    let root = AccountContext::root(&config);
    assert!(root.root);
    assert!(root.account_id.is_none());
    Store::connect(&root).expect("explicit root connection");
}
