use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use rich_studio::logs::Logger;
use rich_studio::metrics::Metrics;
use rich_studio::services::housekeeping;
use rich_studio::settings::{Settings, SettingsError};
use rich_studio::spec::source::Spec;
use rich_studio::store::db;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The server-mode configuration from the Python reference `tests/test_server.py`.
const SERVER: [(&str, &str); 3] = [
    ("STUDIO_MODE", "server"),
    ("STUDIO_URL", "https://studio.example.com"),
    ("TG_BOT_TOKEN", "1:x"),
];

fn settings_from(values: &[(&str, &str)]) -> Result<Settings, SettingsError> {
    let values: BTreeMap<String, String> = values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    Settings::from_map(&values)
}

fn problems(values: &[(&str, &str)]) -> Vec<String> {
    match settings_from(values) {
        Ok(_) => panic!("settings must be rejected"),
        Err(error) => error.problems,
    }
}

fn data_settings(directory: &Path) -> Result<Settings, SettingsError> {
    let directory = directory.to_string_lossy().into_owned();
    settings_from(&[("DATA_DIR", &directory)])
}

fn data_dir_error(result: Result<(), SettingsError>) -> SettingsError {
    match result {
        Ok(()) => panic!("the DATA_DIR check must fail"),
        Err(error) => error,
    }
}

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
    assert_eq!(
        hash,
        rich_studio::hex::encode(&Sha256::digest(secret.as_bytes()))
    );
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

/// A server-mode command with an isolated environment, the way the Python reference builds
/// `Settings.from_env(SERVER)` with `DATA_DIR` in a temporary folder.
fn server_command(data: &Path, port: Option<u16>, api_key: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rich-studio"));
    command
        .arg("serve")
        .env_clear()
        .env(
            "SystemRoot",
            std::env::var("SystemRoot").unwrap_or_default(),
        )
        .env("DATA_DIR", data)
        .env("STUDIO_MODE", "server")
        .env("STUDIO_URL", "https://studio.example.com")
        .env("TG_BOT_TOKEN", "1:x")
        .env("TG_API_BASE", "http://127.0.0.1:9")
        .env("TG_UPDATES", "off")
        .env("STUDIO_BACKGROUND", "off")
        .env("SPEC_PIN", "on")
        .env(
            "STUDIO_STATIC_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("app/static"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(port) = port {
        command.env("STUDIO_BIND", format!("127.0.0.1:{port}"));
    }
    if let Some(api_key) = api_key {
        command.env("STUDIO_API_KEY", api_key);
    }
    command
}

#[test]
fn every_bad_value_is_named_at_once() {
    // Port of tests/test_server.py::test_every_bad_value_is_named_at_once.
    let found = problems(&[
        ("LOG_FILES", "x"),
        ("SESSION_DAYS", "-1"),
        ("LOG_LEVEL", "loud"),
        ("STUDIO_MODE", "prod"),
    ]);
    let names: Vec<&str> = found
        .iter()
        .map(|problem| problem.split('=').next().unwrap_or_default())
        .collect();
    assert_eq!(
        names,
        ["STUDIO_MODE", "SESSION_DAYS", "LOG_LEVEL", "LOG_FILES"]
    );
}

#[test]
fn secrets_come_from_files() -> TestResult {
    // Port of tests/test_server.py::test_secrets_come_from_files.
    let directory = TempDir::new()?;
    let token = directory.path().join("token");
    fs::write(&token, "123:FROM_FILE\n")?;
    let token = token.to_string_lossy().into_owned();
    let settings = settings_from(&[("TG_BOT_TOKEN_FILE", &token)])?;
    assert_eq!(settings.bot_token, "123:FROM_FILE");

    let missing = directory
        .path()
        .join("missing")
        .to_string_lossy()
        .into_owned();
    let missing = problems(&[("STUDIO_API_KEY_FILE", &missing)]);
    assert!(
        missing[0].starts_with("STUDIO_API_KEY_FILE"),
        "{}",
        missing[0]
    );

    let both = problems(&[("TG_BOT_TOKEN", "1:x"), ("TG_BOT_TOKEN_FILE", &token)]);
    assert!(both[0].contains("оставьте одно"), "{}", both[0]);
    Ok(())
}

#[test]
fn server_mode_needs_https_and_a_bot_token() -> TestResult {
    // Port of tests/test_server.py::test_server_mode_needs_https_and_a_bot_token.
    assert_eq!(problems(&[("STUDIO_MODE", "server")]).len(), 2);
    let settings = settings_from(&SERVER)?;
    assert!(settings.allowed_hosts.contains("studio.example.com"));
    assert!(settings.spec_pinned);
    let mut unpinned = SERVER.to_vec();
    unpinned.push(("SPEC_PIN", "off"));
    assert!(!settings_from(&unpinned)?.spec_pinned);
    assert!(!settings_from(&[])?.spec_pinned);
    Ok(())
}

#[tokio::test]
async fn server_mode_refuses_to_start_without_a_token() -> TestResult {
    // Port of tests/test_server.py::test_server_mode_refuses_to_start_without_a_token: in Rust
    // tokens live in the database, so the refusal happens while serving, not while parsing.
    let directory = TempDir::new()?;
    let data = directory.path().join("data");
    let mut refused = Server(server_command(&data, None, None).spawn()?);
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = refused.0.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            return Err("server without a token did not exit".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(!status.success());
    let mut message = String::new();
    refused
        .0
        .stderr
        .take()
        .ok_or("refused server stderr")?
        .read_to_string(&mut message)?;
    assert!(
        message.contains("STUDIO_MODE=server") && message.contains("токен"),
        "{message}"
    );

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let mut server = Server(server_command(&data, Some(address.port()), Some("k")).spawn()?);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let base = format!("http://{address}");
    let start = Instant::now();
    loop {
        if let Some(status) = server.0.try_wait()? {
            let mut message = String::new();
            if let Some(mut stderr) = server.0.stderr.take() {
                let _ = stderr.read_to_string(&mut message);
            }
            return Err(format!("server with an api key exited ({status}): {message}").into());
        }
        if let Ok(response) = client
            .get(format!("{base}/healthz"))
            .header("host", "studio.example.com")
            .send()
            .await
            && response.status().is_success()
        {
            break;
        }
        if start.elapsed() > Duration::from_secs(15) {
            return Err("server with an api key did not become ready".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

#[test]
fn unwritable_data_dir_check() -> TestResult {
    // Port of tests/test_server.py::test_unwritable_data_dir_check. The Python test replaced
    // Path.write_bytes with a failing function; here the probe path is occupied by a directory
    // so the write fails the same way without monkeypatching the filesystem.
    let directory = TempDir::new()?;
    let settings = data_settings(&directory.path().join("data"))?;
    rich_studio::runtime::check_data_dir(&settings)?; // made when missing
    assert!(settings.data_dir.is_dir());

    fs::create_dir(settings.data_dir.join(".write-test"))?;
    let error = data_dir_error(rich_studio::runtime::check_data_dir(&settings));
    assert!(
        error.problems[0].contains("нет записи в DATA_DIR"),
        "{}",
        error.problems[0]
    );
    Ok(())
}

#[test]
fn a_locked_file_inside_data_dir_is_found() -> TestResult {
    // Port of tests/test_server.py::test_a_locked_file_inside_data_dir_is_found. The Python test
    // faked os.access for studio.log; a read-only file reproduces the root-owned leftover.
    let directory = TempDir::new()?;
    let settings = data_settings(directory.path())?;
    fs::create_dir(directory.path().join("logs"))?;
    fs::write(directory.path().join("logs/studio.log"), b"")?;
    fs::write(directory.path().join("studio.db"), b"")?;
    rich_studio::runtime::check_data_dir(&settings)?;

    let locked = directory.path().join("logs/studio.log");
    let original = fs::metadata(&locked)?.permissions();
    let mut readonly = original.clone();
    readonly.set_readonly(true);
    fs::set_permissions(&locked, readonly)?;
    let error = data_dir_error(rich_studio::runtime::check_data_dir(&settings));
    fs::set_permissions(&locked, original)?;
    assert!(
        error.problems[0].contains("studio.log не записывается"),
        "{}",
        error.problems[0]
    );
    Ok(())
}

#[test]
fn permission_diagnostic_matches_the_platform() -> TestResult {
    // Port of tests/test_server.py::test_permission_diagnostic_matches_the_platform. The Python
    // test simulated POSIX by setting os.getuid/os.getgid; the identity is the seam here.
    let directory = TempDir::new()?;
    let settings = data_settings(directory.path())?;
    let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let posix = rich_studio::runtime::data_dir_diagnostic(&settings, &error, Some((10001, 10001)));
    assert!(posix.contains("chown -R 10001:10001"), "{posix}");
    let windows = rich_studio::runtime::data_dir_diagnostic(&settings, &error, None);
    assert!(windows.contains("безопасности Windows"), "{windows}");
    Ok(())
}

#[tokio::test]
async fn pinned_spec_is_the_bundled_one_and_never_downloads() -> TestResult {
    // Port of tests/test_server.py::test_pinned_spec_is_the_bundled_one_and_never_downloads.
    let directory = TempDir::new()?;
    let server = RecordingServer::start()?;
    let data_dir = directory.path().to_string_lossy().into_owned();
    let spec_url = format!("{}/spec", server.base);
    let settings = settings_from(&[
        ("DATA_DIR", &data_dir),
        ("SPEC_PIN", "on"),
        ("SPEC_URL", &spec_url),
    ])?;
    let mut newer = Spec::load_initial(&settings).data.clone();
    newer["version"] = json!("Bot API 99.0");
    fs::write(
        directory.path().join("api.json"),
        serde_json::to_vec(&newer)?,
    )?;

    let mut spec = Spec::load_initial(&settings);
    assert_eq!(spec.source, "bundled");
    assert_ne!(spec.version(), "Bot API 99.0");
    let report = spec.refresh(&settings, true).await;
    assert_eq!(report["pinned"], true);
    assert_eq!(report["updated"], false);
    assert!(spec.last_check.is_none());
    assert!(
        server.seen().is_empty(),
        "a pinned spec must not be downloaded: {:?}",
        server.seen()
    );
    Ok(())
}

#[tokio::test]
async fn dead_mans_switch() -> TestResult {
    // Port of tests/test_server.py::test_dead_mans_switch.
    let directory = TempDir::new()?;
    let server = RecordingServer::start()?;
    let mut settings = data_settings(directory.path())?;
    let logger = Logger::new(&settings, Arc::new(Metrics::default()));

    housekeeping::ping(&settings, "ok", &logger).await; // no URL: nothing to do
    settings.deadman_url = format!("{}/ping/secret-uuid", server.base);
    housekeeping::ping(&settings, "warn", &logger).await;
    housekeeping::ping(&settings, "fail", &logger).await;
    assert_eq!(
        server.seen().join(","),
        "/ping/secret-uuid,/ping/secret-uuid/fail"
    );

    settings.deadman_url = format!("{}/down", server.base);
    housekeeping::ping(&settings, "ok", &logger).await; // a failed ping is a warning
    let lines = logger.read("WARNING", 50, None, "rich-studio.health", "")?;
    let text = serde_json::to_string(&lines)?;
    assert!(text.contains("dead man's switch not reached"), "{text}");
    assert!(text.contains("deadman.error"), "{text}");
    assert!(
        !text.contains(&server.base),
        "the URL is the secret and must not be logged: {text}"
    );
    Ok(())
}

/// A local HTTP peer for the ping tests: records request paths and answers `/down` with 500.
struct RecordingServer {
    base: String,
    seen: Arc<Mutex<Vec<String>>>,
    stopping: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl RecordingServer {
    fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base = format!("http://{}", listener.local_addr()?);
        let stopping = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let flag = stopping.clone();
        let recorded = seen.clone();
        let task = std::thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut request = Vec::new();
                        let mut buffer = [0u8; 4096];
                        while request.len() < 16_384
                            && !request.windows(4).any(|part| part == b"\r\n\r\n")
                        {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(length) => request.extend_from_slice(&buffer[..length]),
                            }
                        }
                        let path = String::from_utf8_lossy(&request)
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_owned();
                        let failed = path.ends_with("/down");
                        if let Ok(mut paths) = recorded.lock() {
                            paths.push(path);
                        }
                        let response = if failed {
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        };
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            base,
            seen,
            stopping,
            task: Some(task),
        })
    }

    fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .map(|paths| paths.clone())
            .unwrap_or_default()
    }
}

impl Drop for RecordingServer {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
