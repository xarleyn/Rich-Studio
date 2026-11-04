mod cli;
mod import_login;

use std::process::ExitCode;
use std::time::Duration;

use rich_studio::{
    AppState, build_router, runtime,
    settings::Settings,
    store::{
        auth,
        db::{self, Store},
    },
};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code as u8),
        Err(error) => {
            eprintln!("Rich Studio: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let settings = Settings::from_env()?;
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let cli::Action::Exit(code) = cli::run(&arguments, &settings).await? {
        return Ok(code);
    }
    let configured = settings.clone();
    tokio::task::spawn_blocking(move || {
        let mut database = Store::connect_root(&configured).map_err(|error| error.to_string())?;
        db::migrate(&mut database).map_err(|error| error.to_string())?;
        if configured.mode == "server"
            && !configured.public
            && configured.api_key.is_empty()
            && !auth::any_tokens(&configured).map_err(|error| error.to_string())?
        {
            return Err(
                "STUDIO_MODE=server требует токен: rich-studio token add NAME --level admin"
                    .to_owned(),
            );
        }
        Ok::<_, String>(())
    })
    .await??;
    if !runtime::static_dir().join("index.html").is_file() {
        return Err(
            "editor resources not found; set STUDIO_STATIC_DIR to the installed static directory"
                .into(),
        );
    }
    let grace = Duration::from_secs_f64(settings.shutdown_grace);
    let listener = tokio::net::TcpListener::bind(settings.bind).await?;
    let state = AppState::new(settings);
    state.logs.install_tracing()?;
    let logs = state.logs.clone();
    let loops = rich_studio::services::loops::Loops::start(state.clone());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(
        listener,
        build_router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = stopped.await;
    });
    let server = std::future::IntoFuture::into_future(server);
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => { result?; }
        signal = runtime::shutdown_signal() => {
            signal?;
            loops.request_stop();
            let _ = stop.send(());
            match tokio::time::timeout(grace, &mut server).await {
                Ok(result) => result?,
                Err(_) => return Err("shutdown grace expired; interrupted sends remain recorded for recovery".into()),
            }
        }
    }
    loops.stop().await?;
    tokio::task::spawn_blocking(move || logs.flush()).await??;
    Ok(0)
}
