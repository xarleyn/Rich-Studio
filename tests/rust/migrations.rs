use std::collections::BTreeMap;
use std::path::Path;

use rich_studio::account_context::AccountContext;
use rich_studio::settings::Settings;
use rich_studio::store::db::{self, Store};
use rusqlite::{Connection, params};
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
    assert_eq!(migrations.len(), db::embedded_migrations().len());
    assert!(migrations.iter().all(|sql| sql.starts_with("-- ")));
    let mut files: Vec<String> = std::fs::read_dir(Path::new(db::MIGRATIONS_DIR))
        .expect("migrations folder")
        .map(|entry| {
            entry
                .expect("migration entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".sql"))
        .collect();
    files.sort();
    for name in &files {
        assert_eq!(
            name.as_bytes().get(4),
            Some(&b'_'),
            "migration file name must be NNNN_name.sql: {name}"
        );
    }
}

#[test]
fn duplicate_or_skipped_migration_numbers_stop_startup() {
    let dir = TempDir::new().expect("temp dir");
    for name in ["0001_a.sql", "0002_b.sql", "0002_c.sql"] {
        std::fs::write(dir.path().join(name), "-- test\nselect 1;").expect("migration fixture");
    }
    let duplicate =
        db::load_migrations(dir.path()).expect_err("a number taken twice must stop startup");
    assert!(
        duplicate.to_string().contains("without gaps"),
        "unexpected duplicate error: {duplicate}"
    );
    std::fs::rename(dir.path().join("0002_c.sql"), dir.path().join("0004_c.sql"))
        .expect("renumber fixture");
    let skipped = db::load_migrations(dir.path()).expect_err("a skipped number must stop startup");
    assert!(
        skipped.to_string().contains("without gaps"),
        "unexpected skipped error: {skipped}"
    );
}

#[test]
fn fresh_database_runs_every_step_with_durable_pragmas() {
    let dir = TempDir::new().expect("temp dir");
    let mut connection = root_connection(dir.path());
    db::migrate(&mut connection).expect("migrate fresh database");

    let version: i64 = connection
        .query_row("pragma user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, db::embedded_migrations().len() as i64);
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
    assert_eq!(version, db::embedded_migrations().len() as i64);
    connection
        .execute_batch("select post_id, chat_ids from drafts;")
        .expect("new draft columns");
}

#[test]
fn failed_step_rolls_back_schema_and_version_then_can_run_again() {
    let dir = TempDir::new().expect("temp dir");
    let mut connection = root_connection(dir.path());
    let done = db::embedded_migrations().len();
    db::migrate(&mut connection).expect("base migrations");
    let mut migrations = db::load_migrations(db::MIGRATIONS_DIR).expect("bundled migrations");
    let broken = "-- test\ncreate table half_done(x);\ninsert into no_such_table values (1);";
    migrations.push(broken.to_owned());

    let error = db::migrate_steps(&mut connection, &migrations).expect_err("broken migration");
    assert!(
        error.to_string().contains("no_such_table"),
        "unexpected migration error: {error}"
    );
    let version: i64 = connection
        .query_row("pragma user_version", [], |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, done as i64);
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
    let version: i64 = connection
        .query_row("pragma user_version", [], |row| row.get(0))
        .expect("schema version after retry");
    assert_eq!(version, done as i64 + 1);
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

#[test]
fn migration_gives_old_rows_the_kind_of_their_content() {
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
    for step in migrations.iter().take(9) {
        connection.execute_batch(step).expect("old migration");
    }
    connection
        .execute_batch("pragma user_version = 9;")
        .expect("old version");
    for (id, kind, payload) in [
        ("a", Some("rich"), r#"{"poll":{"question":"?"}}"#),
        ("b", Some("rich"), r#"{"live_photo":{"caption":"c"}}"#),
        ("c", None, r#"{"paid_media":{"star_count":1}}"#),
        ("d", None, r#"{"rich_message":{"markdown":"x"}}"#),
        ("e", Some("poll"), r#"{"poll":{"question":"?"}}"#),
        ("f", Some("rich"), "not json"),
        (
            "g",
            Some("rich"),
            r#"{"rich_message":{"markdown":"x"},"live_photo":{"caption":"лишнее поле"}}"#,
        ),
    ] {
        connection
            .execute(
                "insert into posts(id, chat_id, payload, status, kind) values (?1, '@c', ?2, 'sent', ?3)",
                params![id, payload, kind],
            )
            .expect("old post row");
    }

    db::migrate(&mut connection).expect("upgrade old database");
    let mut statement = connection
        .prepare("select id, kind from posts order by id")
        .expect("kind query");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("kind rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("kind values");
    assert_eq!(
        rows,
        [
            ("a".to_owned(), "poll".to_owned()),
            ("b".to_owned(), "live_photo".to_owned()),
            ("c".to_owned(), "paid_media".to_owned()),
            ("d".to_owned(), "rich".to_owned()),
            ("e".to_owned(), "poll".to_owned()),
            ("f".to_owned(), "rich".to_owned()),
            ("g".to_owned(), "rich".to_owned()),
        ]
    );
}
