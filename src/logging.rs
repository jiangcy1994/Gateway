//! Tracing initialization. Console mode logs to stdout; Windows service mode
//! logs to a rolling daily file, because a service process has no attached
//! console and stdout is discarded.

use tracing_subscriber::EnvFilter;

/// Default filter when `RUST_LOG` is unset: gateway code at info, everything
/// else only for warnings.
pub const DEFAULT_FILTER: &str = "top_gateway=info,warn";

/// Initializes stdout logging for console mode.
pub fn init_console() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_FILTER.into());
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(windows)]
mod file_appender {
    use super::DEFAULT_FILTER;
    use std::path::Path;
    use tracing_subscriber::EnvFilter;

    /// Guard returned by [`init_file`]; dropping it flushes the non-blocking
    /// file writer, so keep it alive for the whole process (the service shell
    /// holds it until the server has shut down).
    pub struct LogGuard {
        _guard: tracing_appender::non_blocking::WorkerGuard,
    }

    /// Initializes rolling daily file logging under `dir` (`top-gateway.log`,
    /// rotated files get a `.YYYY-MM-DD` suffix). A missing or unwritable
    /// directory is an error: logging is the only way to observe a broken
    /// service, so the caller (service shell) surfaces the failure to the SCM.
    pub fn init_file(dir: &Path) -> Result<LogGuard, std::io::Error> {
        let file = tracing_appender::rolling::daily(dir, "top-gateway.log");
        let (writer, guard) = tracing_appender::non_blocking(file);
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_FILTER.into());
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .init();
        Ok(LogGuard { _guard: guard })
    }
}

#[cfg(windows)]
pub use file_appender::{init_file, LogGuard};
