//! Explicit interactive MTProto authorization. HTTP handlers never call this module.

use grammers_client::SignInError;
use rich_studio::{
    account_context::{AccountContext, Role},
    settings::Settings,
    telegram::import_client::{Connection, encode_session},
};
use std::io::Write as _;
use std::path::Path;

pub async fn run(arguments: &[String], settings: &Settings) -> Result<(), String> {
    let account = account_argument(arguments)?;
    if settings.import_api_id == 0 || settings.import_api_hash.is_empty() {
        return Err(
            "Задайте TG_IMPORT_API_ID и TG_IMPORT_API_HASH (или TG_IMPORT_API_HASH_FILE)."
                .to_owned(),
        );
    }
    let context = account.map_or_else(
        || AccountContext::root(settings),
        |uid| AccountContext::account(settings, uid, Role::Publish),
    );
    let path = rich_studio::services::imports::session_path(settings, &context);
    if path.exists() {
        return Err("Сессия уже настроена. Для замены сначала отзовите её в Telegram и удалите локальный файл.".to_owned());
    }
    let connection = Connection::start(settings, "").await.map_err(|_| {
        "Подключение Telegram не удалось. Проверьте параметры импорта и прокси.".to_owned()
    })?;
    let mut authenticated = false;
    let result = async {
        let phone = prompt("Телефон Telegram (ввод скрыт): ").await?;
        let token = connection
            .client
            .request_login_code(&phone, &settings.import_api_hash)
            .await
            .map_err(|_| "Не удалось запросить код Telegram.".to_owned())?;
        let code = prompt("Код Telegram (ввод скрыт): ").await?;
        match connection.client.sign_in(&token, &code).await {
            Ok(_) => {}
            Err(SignInError::PasswordRequired(password_token)) => {
                let password = prompt("Пароль двухэтапной аутентификации: ").await?;
                connection
                    .client
                    .check_password(password_token, password)
                    .await
                    .map_err(|_| {
                        "Telegram отклонил пароль двухэтапной аутентификации.".to_owned()
                    })?;
            }
            Err(_) => {
                return Err(
                    "Telegram отклонил вход. Проверьте код и повторите настройку.".to_owned(),
                );
            }
        }
        authenticated = true;
        let user = connection
            .client
            .get_me()
            .await
            .map_err(|_| "Не удалось проверить учётку Telegram.".to_owned())?;
        if user.is_bot() || account.is_some_and(|expected| user.id().bare_id() != Some(expected)) {
            return Err("Вход выполнен другой учёткой. Сессия не сохранена.".to_owned());
        }
        let encoded = encode_session(&connection.session, connection.ipv6)
            .map_err(|_| "Не удалось сохранить совместимую сессию Telegram.".to_owned())?;
        save_session(&path, &encoded).map_err(|_| {
            "Сессия не записана: проверьте права каталога или существующее подключение.".to_owned()
        })?;
        Ok(())
    }
    .await;
    if result.is_err() && authenticated {
        let _ = connection.client.sign_out().await;
    }
    connection.close().await;
    result?;
    println!("Подключение импорта сохранено. Секрет сессии не входит в экспорт студии.");
    Ok(())
}

fn account_argument(arguments: &[String]) -> Result<Option<i64>, String> {
    match arguments {
        [] => Ok(None),
        [flag, value] if flag == "--account" => value
            .parse::<i64>()
            .ok()
            .filter(|uid| *uid > 0 && *uid < (1i64 << 52))
            .map(Some)
            .ok_or_else(|| "--account должен быть положительным Telegram ID".to_owned()),
        _ => Err("usage: rich-studio import-login [--account TELEGRAM_ID]".to_owned()),
    }
}

async fn prompt(label: &'static str) -> Result<String, String> {
    tokio::task::spawn_blocking(move || rpassword::prompt_password(label))
        .await
        .map_err(|_| "Ввод прерван.".to_owned())?
        .map_err(|_| {
            "Скрытый ввод недоступен: запустите import-login в интерактивном терминале.".to_owned()
        })
}

fn save_session(path: &Path, encoded: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(error) = file
        .write_all(encoded.as_bytes())
        .and_then(|()| file.sync_all())
    {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_argument_rejects_ambiguous_or_invalid_targets() {
        assert_eq!(account_argument(&[]).expect("owner"), None);
        assert_eq!(
            account_argument(&["--account".to_owned(), "42".to_owned()]).expect("account"),
            Some(42)
        );
        for value in ["0", "-1", "../42", "4503599627370496"] {
            assert!(account_argument(&["--account".to_owned(), value.to_owned()]).is_err());
        }
    }
    #[test]
    fn session_write_is_exclusive_and_private() {
        let directory = tempfile::TempDir::new().expect("directory");
        let path = directory.path().join("telegram-import.session.txt");
        save_session(&path, "original").expect("first write");
        assert!(save_session(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("session"), "original");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
