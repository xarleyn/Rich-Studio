//! Operator commands run in the same executable as the HTTP server.

use std::time::Duration;

use rich_studio::services::auth::secret;
use rich_studio::settings::Settings;
use rich_studio::store::auth;

pub enum Action {
    Serve,
    Exit(i32),
}

pub async fn run(arguments: &[String], settings: &Settings) -> Result<Action, String> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return Ok(Action::Serve);
    };
    match command {
        "serve" if arguments.len() == 1 => Ok(Action::Serve),
        "openapi" if arguments.len() == 1 => {
            let schema = rich_studio::api::openapi::document()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&schema).map_err(|error| error.to_string())?
            );
            Ok(Action::Exit(0))
        }
        "mcp-contract" if arguments.len() == 1 => {
            let tools = rich_studio::mcp_tools::definitions().map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&tools).map_err(|error| error.to_string())?
            );
            Ok(Action::Exit(0))
        }
        "--version" | "version" if arguments.len() == 1 => {
            println!("rich-studio {}", env!("CARGO_PKG_VERSION"));
            Ok(Action::Exit(0))
        }
        "--help" | "-h" | "help" => {
            println!(
                "Rich Studio\n  rich-studio [serve]\n  rich-studio token add NAME [--level read|draft|publish|admin]\n  rich-studio token list\n  rich-studio token revoke NAME\n  rich-studio import-login [--account TELEGRAM_ID]\n  rich-studio healthcheck [--url URL]\n  rich-studio request METHOD /path [JSON]\n  rich-studio openapi\n  rich-studio mcp-contract\n\nDATA_DIR and the existing STUDIO_*/TG_* environment configure the server.\nrequest uses STUDIO_TOKEN (or TOKEN), never a command-line secret."
            );
            Ok(Action::Exit(0))
        }
        "token" => token(&arguments[1..], settings).map(Action::Exit),
        "import-login" => {
            crate::import_login::run(&arguments[1..], settings).await?;
            Ok(Action::Exit(0))
        }
        "healthcheck" => {
            let url = match &arguments[1..] {
                [] => format!("{}/readyz", local_origin(settings)),
                [flag, url] if flag == "--url" => url.clone(),
                _ => return Err("usage: rich-studio healthcheck [--url URL]".to_owned()),
            };
            let response = client()?.get(url).send().await.map_err(http_error)?;
            if !response.status().is_success() {
                return Err(format!("readiness returned HTTP {}", response.status()));
            }
            Ok(Action::Exit(0))
        }
        "request" => {
            if !(3..=4).contains(&arguments.len())
                || !arguments[2].starts_with('/')
                || arguments[2].starts_with("//")
            {
                return Err("usage: rich-studio request METHOD /path [JSON]".to_owned());
            }
            let method = arguments[1]
                .parse::<reqwest::Method>()
                .map_err(|_| "invalid HTTP method".to_owned())?;
            let mut request = client()?.request(
                method,
                format!("{}{}", local_origin(settings), arguments[2]),
            );
            if let Ok(token) = std::env::var("STUDIO_TOKEN").or_else(|_| std::env::var("TOKEN")) {
                request = request.bearer_auth(token);
            }
            if let Some(body) = arguments.get(3) {
                let body: serde_json::Value = serde_json::from_str(body)
                    .map_err(|_| "request body is not JSON".to_owned())?;
                request = request.json(&body);
            }
            let response = request.send().await.map_err(http_error)?;
            let status = response.status();
            let text = response.text().await.map_err(http_error)?;
            if !status.is_success() {
                return Err(format!("request returned HTTP {status}"));
            }
            println!("{text}");
            Ok(Action::Exit(0))
        }
        _ => Err("unknown command; run rich-studio --help".to_owned()),
    }
}

fn token(arguments: &[String], settings: &Settings) -> Result<i32, String> {
    match arguments {
        [action, name, rest @ ..] if action == "add" => {
            if name.is_empty()
                || name.len() > 40
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            {
                return Err("имя: латиница, цифры, . _ -, до 40 символов".to_owned());
            }
            let level = match rest {
                [] => "draft",
                [flag, level] if flag == "--level" => level,
                _ => return Err("usage: rich-studio token add NAME [--level LEVEL]".to_owned()),
            };
            if !["read", "draft", "publish", "admin"].contains(&level) {
                return Err("уровень: read, draft, publish или admin".to_owned());
            }
            let token = secret("rs_");
            auth::add_token(settings, name, &token, level).map_err(|_| {
                format!("не удалось создать токен {name}: проверьте имя и доступность базы")
            })?;
            println!("{token}");
            eprintln!("# {name}, уровень {level}. Сохраните сейчас: студия хранит только хэш.");
            Ok(0)
        }
        [action] if action == "list" => {
            for token in auth::list_tokens(settings).map_err(|error| error.to_string())? {
                println!(
                    "{:<24} {:<8} {:<12} последний раз: {}",
                    token["name"].as_str().unwrap_or_default(),
                    token["level"].as_str().unwrap_or_default(),
                    if token["revoked_at"].is_null() {
                        "действует"
                    } else {
                        "отозван"
                    },
                    token["last_used_at"]
                        .as_i64()
                        .map_or_else(|| "—".to_owned(), |time| time.to_string())
                );
            }
            Ok(0)
        }
        [action, name] if action == "revoke" => {
            if auth::revoke_token(settings, name).map_err(|error| error.to_string())? {
                Ok(0)
            } else {
                eprintln!("действующего токена {name} нет");
                Ok(1)
            }
        }
        _ => Err("usage: rich-studio token add|list|revoke".to_owned()),
    }
}

fn local_origin(settings: &Settings) -> String {
    let mut address = settings.bind;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    format!("http://{address}")
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(http_error)
}

fn http_error(error: reqwest::Error) -> String {
    error.without_url().to_string()
}
