//! Filesystem resources for both source runs and relocatable installations.

use std::path::PathBuf;

/// Installed binaries use an adjacent `static` directory. Developers may run from the repository
/// root; explicit configuration always wins, including when it names a missing directory.
pub fn static_dir() -> PathBuf {
    if let Some(configured) = std::env::var_os("STUDIO_STATIC_DIR") {
        return PathBuf::from(configured);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        let installed = parent.join("static");
        if installed.is_dir() {
            return installed;
        }
    }
    PathBuf::from("app/static")
}

/// Both Ctrl-C and container SIGTERM initiate the same graceful server drain.
pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
