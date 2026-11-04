use super::download::persist_cache;
use super::summary::{consts, gaps};
use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tempfile::TempDir;

struct Server {
    url: String,
    stopping: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(body: &Value) -> Result<Self, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/spec", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let flag = stopping.clone();
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let task = std::thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                        let mut buffer = [0; 4096];
                        let mut request = Vec::new();
                        while request.len() < 16_384
                            && !request.windows(4).any(|part| part == b"\r\n\r\n")
                        {
                            match stream.read(&mut buffer) {
                                Ok(0) | Err(_) => break,
                                Ok(length) => request.extend_from_slice(&buffer[..length]),
                            }
                        }
                        if !request.windows(4).any(|part| part == b"\r\n\r\n") {
                            continue;
                        }
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
            url,
            stopping,
            task: Some(task),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
fn fixture(version: &str) -> Value {
    json!({"version":version,"types":{"InputRichBlock":{"subtypes":[]}},"methods":{"sendRichMessage":{"fields":[]}}})
}
fn config(directory: &TempDir, server: &Server) -> Result<Settings, Box<dyn std::error::Error>> {
    Ok(Settings::from_map(&std::collections::BTreeMap::from([
        (
            "DATA_DIR".to_owned(),
            directory.path().to_string_lossy().into_owned(),
        ),
        ("SPEC_URL".to_owned(), server.url.clone()),
    ]))?)
}

#[tokio::test]
async fn history_write_failure_preserves_old_cache_bytes_and_restart_version()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let server = Server::new(&fixture("Bot API 99.0"))?;
    let settings = config(&directory, &server)?;
    let old = serde_json::to_vec_pretty(&fixture("Bot API 98.0"))?;
    fs::write(directory.path().join("api.json"), &old)?;
    fs::create_dir(directory.path().join("spec_changes.json"))?;
    let mut spec = Spec::load_initial(&settings);
    let report = spec.refresh(&settings, true).await;
    assert_eq!(report["updated"], false);
    assert!(report["error"].is_string());
    assert_eq!(spec.version(), "Bot API 98.0");
    assert_eq!(fs::read(directory.path().join("api.json"))?, old);
    assert_eq!(Spec::load_initial(&settings).version(), "Bot API 98.0");
    assert!(directory.path().join("spec_changes.json").is_dir());
    assert!(
        !fs::read_dir(directory.path())?
            .filter_map(Result::ok)
            .any(|file| file.file_name().to_string_lossy().ends_with(".tmp"))
    );
    Ok(())
}

#[tokio::test]
async fn unchanged_refresh_returns_current_empty_diff_and_retains_history()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let mut new = fixture("Bot API 99.0");
    new["methods"]["sendRichMessage"]["fields"] =
        json!([{"name":"new_option","types":["Boolean"],"required":false}]);
    let server = Server::new(&new)?;
    let settings = config(&directory, &server)?;
    fs::write(
        directory.path().join("api.json"),
        serde_json::to_vec(&fixture("Bot API 98.0"))?,
    )?;
    let mut spec = Spec::load_initial(&settings);
    let first = spec.refresh(&settings, true).await;
    assert_eq!(first["updated"], true);
    assert!(!first["changes"].as_array().ok_or("changes")?.is_empty());
    let history = spec.changes.clone();
    let second = spec.refresh(&settings, true).await;
    assert_eq!(second["updated"], false);
    assert_eq!(second["changes"], json!([]), "second refresh: {second}");
    assert_eq!(spec.changes, history);
    assert_eq!(Spec::load_initial(&settings).data, new);
    Ok(())
}

#[test]
fn failed_cache_commit_restores_previous_history() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let history = b"[ { \"from\": \"previous\" } ]\n";
    fs::write(directory.path().join("spec_changes.json"), history)?;
    fs::create_dir(directory.path().join("api.json"))?;
    assert!(persist_cache(directory.path(), "{}", "[]").is_err());
    assert_eq!(
        fs::read(directory.path().join("spec_changes.json"))?,
        history
    );
    assert!(directory.path().join("api.json").is_dir());
    assert!(
        !fs::read_dir(directory.path())?
            .filter_map(Result::ok)
            .any(|file| file.file_name().to_string_lossy().ends_with(".tmp"))
    );
    Ok(())
}

#[test]
fn gaps_report_a_new_rich_block_and_the_bundled_types_are_complete() {
    let bundled = serde_json::from_str::<Value>(BUNDLED).expect("bundled spec");
    let mut types = bundled["types"].clone();
    assert!(consts(&types, "InputRichBlock").len() >= 20);
    assert!(consts(&types, "RichText").len() >= 20);
    types["InputRichBlockHologram"] = json!({
        "name":"InputRichBlockHologram",
        "fields":[{
            "name":"type",
            "types":["String"],
            "required":true,
            "description":"Type of the block, always “hologram”"
        }]
    });
    types["InputRichBlock"]["subtypes"]
        .as_array_mut()
        .expect("block subtypes")
        .push(json!("InputRichBlockHologram"));
    let found = gaps(&types)
        .into_iter()
        .map(|gap| {
            (
                gap["part"].as_str().unwrap_or_default().to_owned(),
                gap["name"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(found.contains(&("labels".to_owned(), "hologram".to_owned())));
    assert!(found.contains(&("preview".to_owned(), "hologram".to_owned())));
    assert!(gaps(&bundled["types"]).is_empty());
}
