use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rich_studio::settings::Settings;
use rich_studio::spec::source::Spec;
use rich_studio::store::db;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn bundled_sqlite_contains_the_unix_wal_deadlock_fix() {
    // 3.51.0/3.51.1 invert the Unix inode/global mutex order when the final
    // WAL connection closes while another opens. Observed on the local Linux
    // stand, including scheduler and health connections. Upstream fix:
    // https://www.sqlite.org/releaselog/3_51_2.html
    assert!(
        rusqlite::version_number() >= 3_051_002,
        "SQLite {} contains the Unix WAL close/open deadlock",
        rusqlite::version()
    );
}

#[test]
fn wal_connections_open_and_close_concurrently_without_hanging() -> TestResult {
    const CHILD: &str = "RICH_STUDIO_WAL_OPEN_CLOSE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let directory = TempDir::new()?;
        let path = directory.path().join("wal-open-close.db");
        let connection = Connection::open(&path)?;
        connection.execute_batch(
            "pragma journal_mode=WAL; create table fixture(value integer); insert into fixture values(0);",
        )?;
        drop(connection);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut workers = Vec::new();
        for worker in 0..4 {
            let path = path.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || -> rusqlite::Result<()> {
                barrier.wait();
                for iteration in 0..1000 {
                    let flags = if worker == 0 {
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    } else {
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    } | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
                    let connection = Connection::open_with_flags(&path, flags)?;
                    connection.busy_timeout(Duration::from_secs(5))?;
                    let _: i64 =
                        connection.query_row("select value from fixture", [], |row| row.get(0))?;
                    if worker == 0 && iteration % 7 == 0 {
                        connection.execute("update fixture set value=value+1", [])?;
                    }
                    // No persistent connection: exercise the final WAL close.
                    drop(connection);
                    std::thread::yield_now();
                }
                Ok(())
            }));
        }
        for worker in workers {
            worker.join().map_err(|_| "WAL worker panicked")??;
        }
        return Ok(());
    }

    // A native SQLite mutex deadlock cannot be cancelled by a Tokio timeout.
    // Bound it in a child process so this regression cannot hang the test runner.
    let mut child = Server(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "wal_connections_open_and_close_concurrently_without_hanging",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.0.try_wait()? {
            assert!(status.success(), "concurrent WAL open/close failed");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("concurrent WAL open/close hung for 45 seconds".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn embedded_migrations_are_exactly_the_existing_files() -> TestResult {
    assert_eq!(
        db::embedded_migrations(),
        db::load_migrations(db::MIGRATIONS_DIR)?
    );
    let mut connection = Connection::open_in_memory()?;
    db::migrate(&mut connection)?;
    assert_eq!(
        connection.query_row("pragma user_version", [], |row| row.get::<_, i64>(0))?,
        db::embedded_migrations().len() as i64
    );
    Ok(())
}

#[test]
fn embedded_spec_remains_available_without_a_cache() -> TestResult {
    let directory = TempDir::new()?;
    let settings = Settings::from_map(&BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
        ("SPEC_PIN".to_owned(), "on".to_owned()),
    ]))?;
    let spec = Spec::load_initial(&settings);
    assert_eq!(spec.source, "bundled");
    assert_ne!(spec.version(), "?");
    assert!(spec.definition("sendRichMessage").is_some());
    assert_eq!(spec.summary()["gaps"], json!([]));
    Ok(())
}

#[tokio::test]
async fn actual_binary_uses_environment_and_relocatable_resources() -> TestResult {
    let directory = TempDir::new()?;
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_rich-studio"));
    let moved = directory
        .path()
        .join(binary.file_name().ok_or("binary filename")?);
    std::fs::copy(binary, &moved)?;
    let static_dir = directory.path().join("static");
    std::fs::create_dir(&static_dir)?;
    std::fs::write(
        static_dir.join("index.html"),
        "<!doctype html><title>Relocated studio</title>",
    )?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let data = directory.path().join("configured-data");
    let child = Command::new(&moved)
        .arg("serve")
        .current_dir(directory.path())
        .env_clear()
        .env(
            "SystemRoot",
            std::env::var("SystemRoot").unwrap_or_default(),
        )
        .env("STUDIO_BIND", address.to_string())
        .env("DATA_DIR", &data)
        .env("TG_API_BASE", "http://127.0.0.1:9")
        .env("TG_UPDATES", "off")
        .env("STUDIO_BACKGROUND", "off")
        .env("SPEC_PIN", "on")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut server = Server(child);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let base = format!("http://{address}");
    let start = Instant::now();
    loop {
        if server.0.try_wait()?.is_some() {
            return Err("relocated server exited before readiness".into());
        }
        if let Ok(response) = client.get(format!("{base}/readyz")).send().await
            && response.status().is_success()
        {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            return Err("server readiness timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        client
            .get(&base)
            .send()
            .await?
            .text()
            .await?
            .contains("Relocated studio")
    );
    let draft: Value = client
        .post(format!("{base}/api/drafts"))
        .json(
            &json!({"title":"environment proof", "post":{"rich_message":{"html":"<p>saved</p>"}}}),
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert!(draft["id"].is_string());
    assert!(data.join("studio.db").is_file());
    let response = Command::new(&moved)
        .args(["healthcheck", "--url", &format!("{base}/readyz")])
        .output()?;
    assert!(response.status.success(), "healthcheck failed");
    Ok(())
}

#[test]
fn token_cli_preserves_legacy_hashes_and_revokes_sessions() -> TestResult {
    let directory = TempDir::new()?;
    let run = |arguments: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rich-studio"))
            .args(arguments)
            .env("DATA_DIR", directory.path())
            .env("STUDIO_MODE", "local")
            .env("STUDIO_PUBLIC", "off")
            .output()
    };
    let created = run(&["token", "add", "portable-agent", "--level", "draft"])?;
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let secret = String::from_utf8(created.stdout)?.trim().to_owned();
    assert!(secret.starts_with("rs_") && secret.len() >= 46);
    let database = Connection::open(directory.path().join("studio.db"))?;
    let hash: String = database.query_row(
        "select hash from tokens where name='portable-agent'",
        [],
        |row| row.get(0),
    )?;
    use sha2::{Digest as _, Sha256};
    assert_eq!(hash, format!("{:x}", Sha256::digest(secret.as_bytes())));
    database.execute("insert into sessions(hash, token_name, created_at, expires_at) values ('session','portable-agent',0,9999999999)", [])?;
    let listed = run(&["token", "list"])?;
    assert!(listed.status.success());
    assert!(!String::from_utf8_lossy(&listed.stdout).contains(&secret));
    assert!(
        run(&["token", "revoke", "portable-agent"])?
            .status
            .success()
    );
    assert_eq!(
        database.query_row("select count(*) from sessions", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    assert!(
        !run(&["token", "revoke", "portable-agent"])?
            .status
            .success()
    );
    assert!(!run(&["token", "add", "bad/name"])?.status.success());
    Ok(())
}
