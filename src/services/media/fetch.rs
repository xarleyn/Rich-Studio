//! Скачивание по ссылке: временный `.part`, пять редиректов, проверка адресов.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use url::Url;
use uuid::Uuid;

use super::network::{internal_name, public_address, refused, system_proxy};
use super::{MediaError, MediaService, describe};

struct FetchFile {
    path: PathBuf,
    target: Option<PathBuf>,
    file: Option<fs::File>,
    keep: bool,
}
impl Drop for FetchFile {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.keep {
            let _ = fs::remove_file(&self.path);
            if let Some(target) = &self.target {
                let _ = fs::remove_file(target);
            }
        }
    }
}

impl MediaService {
    pub async fn fetch(
        &self,
        url: &str,
        filename: Option<&str>,
        public_only: bool,
        allow_private: bool,
    ) -> Result<Value, MediaError> {
        let mut url = Url::parse(url)
            .map_err(|_| MediaError::BadRequest("url: нужна ссылка http(s)".to_owned()))?;
        let suggested = filename
            .map(str::to_owned)
            .or_else(|| {
                url.path_segments()
                    .and_then(Iterator::last)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "file".to_owned());
        let (name, part) = self
            .run(move |service| {
                let name = service.free_name(&suggested);
                fs::create_dir_all(&service.context.media_dir)?;
                let path = service
                    .context
                    .media_dir
                    .join(format!(".{name}.{}.part", Uuid::new_v4().simple()));
                let mut part = FetchFile {
                    path,
                    target: None,
                    file: None,
                    keep: false,
                };
                part.file = Some(
                    fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&part.path)?,
                );
                Ok((name, part))
            })
            .await?;
        let mut part = self
            .fetch_to_part(&mut url, part, public_only, allow_private)
            .await?;
        self.run(move |service| {
            if let Some(mut file) = part.file.take() {
                file.flush()?;
            }
            let final_name = service.free_name(&name);
            let target = service.context.media_dir.join(&final_name);
            part.target = Some(target.clone());
            fs::rename(&part.path, target)?;
            part.keep = true;
            drop(part);
            Ok(describe(&final_name))
        })
        .await
    }

    async fn fetch_to_part(
        &self,
        url: &mut Url,
        mut part: FetchFile,
        public_only: bool,
        allow_private: bool,
    ) -> Result<FetchFile, MediaError> {
        for _ in 0..=5 {
            if !["http", "https"].contains(&url.scheme()) || url.host_str().is_none() {
                return Err(MediaError::BadRequest(
                    "url: нужна ссылка http(s)".to_owned(),
                ));
            }
            let host = url.host_str().unwrap_or_default().to_owned();
            if internal_name(&host) {
                return Err(refused(&host));
            }
            let port = url.port_or_known_default().unwrap_or(80);
            let proxied = system_proxy(url.scheme(), &host);
            let addresses = match tokio::net::lookup_host((host.as_str(), port)).await {
                Ok(addresses) => addresses.collect::<Vec<_>>(),
                Err(_) if proxied => Vec::new(),
                Err(_) => {
                    return Err(MediaError::Fetch(format!(
                        "не удалось скачать: адрес {host} не найден"
                    )));
                }
            };
            let checked = if addresses.is_empty() {
                None
            } else if public_only || !allow_private || self.context.account_id.is_some() {
                Some(public_address(
                    &addresses.iter().map(SocketAddr::ip).collect::<Vec<_>>(),
                    &host,
                )?)
            } else {
                None
            };
            let mut builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("RichStudio/0.1.0 (media fetch)");
            if let Some(address) = checked {
                builder = builder.resolve(&host, SocketAddr::new(address, port));
            }
            let client = builder
                .build()
                .map_err(|_| MediaError::Fetch("не удалось настроить скачивание".into()))?;
            let response = client
                .get(url.clone())
                .send()
                .await
                .map_err(|_| MediaError::Fetch("не удалось скачать: сеть недоступна".into()))?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        MediaError::Fetch("не удалось скачать: пустой redirect".to_owned())
                    })?;
                *url = url
                    .join(location)
                    .map_err(|_| MediaError::BadRequest("url: неверный redirect".to_owned()))?;
                continue;
            }
            if response.status() != reqwest::StatusCode::OK {
                return Err(MediaError::Fetch(format!(
                    "не удалось скачать: HTTP {}",
                    response.status()
                )));
            }
            let mut response = response;
            let mut size = 0usize;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| MediaError::Fetch("не удалось скачать: передача прервана".into()))?
            {
                size += chunk.len();
                if size > self.max_upload_bytes {
                    return Err(MediaError::TooLarge);
                }
                part = self
                    .run(move |service| {
                        service.check_capacity(chunk.len())?;
                        part.file
                            .as_mut()
                            .ok_or_else(|| MediaError::Fetch("файл закрыт".into()))?
                            .write_all(&chunk)?;
                        Ok(part)
                    })
                    .await?;
            }
            return Ok(part);
        }
        Err(MediaError::Fetch(
            "не удалось скачать: больше 5 перенаправлений".to_owned(),
        ))
    }
}
