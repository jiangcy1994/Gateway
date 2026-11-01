//! Windows service shell for top-gateway.
//!
//! One binary, two modes:
//!
//! - launched by the service control manager (no recognized argument) → service
//!   mode: config/logs live under `GATEWAY_BASE_DIR` (default
//!   `%ProgramData%\top-gateway`), shutdown comes from `SERVICE_CONTROL_STOP`,
//! - `top-gateway.exe <command>` → management console: `run` starts the same
//!   server in the foreground, `install`/`start`/`stop`/`uninstall`/`status`
//!   manage the registered service.
//!
//! The gateway itself (`lib::run_with_shutdown`) is unchanged by this shell; it
//! only supplies the shutdown trigger from the SCM control handler. Design
//! rationale: ADR-011..013 in `docs/sdd/decisions.md`.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::BoxFuture;
use windows_service::service_control_handler::ServiceStatusHandle;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

use crate::config;
use crate::logging;
use crate::Settings;

/// Registered service name (used with `sc`, `net start`, and the commands
/// below). This is also the SCM service key, so it must not contain spaces.
pub const SERVICE_NAME: &str = "top-gateway";
const SERVICE_DISPLAY_NAME: &str = "top-gateway";
const SERVICE_DESCRIPTION: &str =
    "Path-prefix gateway aggregator with admin REST API (axum + tokio)";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// winapi error codes we surface as friendly messages instead of raw errors.
/// 1060 = ERROR_SERVICE_DOES_NOT_EXIST, 1062 = ERROR_SERVICE_NOT_ACTIVE,
/// 1063 = ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, 5 = ERROR_ACCESS_DENIED.
const C_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const C_SERVICE_NOT_ACTIVE: i32 = 1062;
const C_NO_SERVICE_CONTROLLER: i32 = 1063;
const C_ACCESS_DENIED: i32 = 5;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub enum ShellError {
    Service(windows_service::Error),
    Io(std::io::Error),
    Message(String),
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // `windows_service::Error`'s own Display drops the OS error code;
            // print the inner io::Error instead so "os error 5" is visible.
            ShellError::Service(windows_service::Error::Winapi(io)) => {
                write!(f, "winapi call failed: {io}")
            }
            ShellError::Service(e) => write!(f, "{e}"),
            ShellError::Io(e) => write!(f, "{e}"),
            ShellError::Message(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ShellError {}

impl From<windows_service::Error> for ShellError {
    fn from(e: windows_service::Error) -> Self {
        match access_denied_hint(&e) {
            Some(msg) => ShellError::Message(msg),
            None => ShellError::Service(e),
        }
    }
}

/// Non-elevated shells get ERROR_ACCESS_DENIED from the SCM on every management
/// command; make that a plain instruction instead of a bare winapi error.
fn access_denied_hint(e: &windows_service::Error) -> Option<String> {
    match e {
        windows_service::Error::Winapi(io) if io.raw_os_error() == Some(C_ACCESS_DENIED) => Some(
            "access denied (os error 5) — service commands require an elevated \
                 (administrator) prompt; run them from an administrator PowerShell"
                .to_string(),
        ),
        _ => None,
    }
}

impl From<std::io::Error> for ShellError {
    fn from(e: std::io::Error) -> Self {
        ShellError::Io(e)
    }
}

/// Entry point used by `main` on Windows: dispatches on the first argument, or
/// runs as a service when there is none (the SCM launches the binary without
/// arguments). Returns the process exit code; `main` is responsible for
/// printing errors and exiting.
pub fn dispatch() -> Result<i32, ShellError> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => start_dispatcher(),
        Some("run") | Some("console") => run_console(),
        Some("install") => install(&args.collect::<Vec<_>>()),
        Some("uninstall") => uninstall(),
        Some("start") => start(),
        Some("stop") => stop(),
        Some("status") => status(),
        Some("help") | Some("--help") | Some("-h") => {
            print_usage();
            Ok(0)
        }
        Some(other) => Err(ShellError::Message(format!(
            "unknown command `{other}` — try `top-gateway help`"
        ))),
    }
}

/// Connects this process to the service control manager. Blocks until the
/// service stops; [`service_main`] is invoked on an SCM thread and performs the
/// exit itself. Failure with `C_NO_SERVICE_CONTROLLER` means the binary was
/// started directly (double-click / plain shell invocation) rather than by the
/// SCM.
fn start_dispatcher() -> Result<i32, ShellError> {
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(0),
        Err(e) => {
            let hint = match &e {
                windows_service::Error::Winapi(io)
                    if io.raw_os_error() == Some(C_NO_SERVICE_CONTROLLER) =>
                {
                    "not started by the service control manager — run from a console with \
                         `top-gateway run`, or install the service first with `top-gateway install`"
                        .to_string()
                }
                _ => format!("{e}"),
            };
            Err(ShellError::Message(format!(
                "cannot start service dispatcher: {hint}"
            )))
        }
    }
}

fn print_usage() {
    println!("top-gateway {VERSION} — Windows service shell");
    println!();
    println!("usage: top-gateway <command>");
    println!();
    println!("commands:");
    println!("  run                    run the gateway in the foreground (Ctrl+C to stop)");
    println!(
        "  install [--autostart]  register the service (config under %ProgramData%\\top-gateway)"
    );
    println!("  uninstall              unregister the service (must be stopped first)");
    println!("  start                  start the registered service");
    println!("  stop                   stop the registered service");
    println!("  status                 show service state, PID and exit code");
    println!("  help                   this message");
    println!();
    println!("without arguments the binary runs as a Windows service (started by the SCM).");
    println!("under the service, GATEWAY_* settings are read from gateway.env in the config");
    println!("directory; in `run` mode they come from the environment as usual.");
}

// ----------------------------------------------------------------- management

fn open_manager(access: ServiceManagerAccess) -> Result<ServiceManager, ShellError> {
    Ok(ServiceManager::local_computer(None::<&str>, access)?)
}

fn open_service(
    manager: &ServiceManager,
    access: ServiceAccess,
) -> Result<windows_service::service::Service, ShellError> {
    manager
        .open_service(SERVICE_NAME, access)
        .map_err(|e| match not_installed(&e) {
            Some(msg) => ShellError::Message(msg),
            None => ShellError::from(e),
        })
}

fn not_installed(e: &windows_service::Error) -> Option<String> {
    match e {
        windows_service::Error::Winapi(io)
            if io.raw_os_error() == Some(C_SERVICE_DOES_NOT_EXIST) =>
        {
            Some(format!("service `{SERVICE_NAME}` is not installed"))
        }
        _ => None,
    }
}

fn install(extra: &[String]) -> Result<i32, ShellError> {
    let autostart = extra.iter().any(|a| a == "--autostart");
    if let Some(bad) = extra.iter().find(|a| a.as_str() != "--autostart") {
        return Err(ShellError::Message(format!(
            "install: unknown option `{bad}` (only --autostart is supported)"
        )));
    }

    let manager =
        open_manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let exe = std::env::current_exe()?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY_NAME),
        service_type: SERVICE_TYPE,
        start_type: if autostart {
            ServiceStartType::AutoStart
        } else {
            ServiceStartType::OnDemand
        },
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None, // runs as LocalSystem
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
    service.set_description(SERVICE_DESCRIPTION)?;

    let base = ensure_service_base_dir(&exe)?;
    println!("service `{SERVICE_NAME}` installed");
    println!(
        "  start type : {}",
        if autostart {
            "automatic (delayed start)"
        } else {
            "manual"
        }
    );
    println!(
        "  config dir : {} (routes.toml / providers.toml / gateway.env)",
        base.display()
    );
    println!(
        "  log file   : {} (top-gateway.log.YYYY-MM-DD, rotated daily)",
        base.display()
    );
    println!();
    print!("next: edit `gateway.env` (set GATEWAY_ADMIN_KEY at minimum), then `top-gateway start`");
    println!();

    Ok(0)
}

/// Creates the service config directory (default `%ProgramData%\top-gateway`)
/// and seeds config files that are missing there, copying from the directory
/// the executable lives in (a typical deployment ships the exe and its config
/// side by side). Existing files are never overwritten.
fn ensure_service_base_dir(exe: &Path) -> Result<PathBuf, ShellError> {
    let base = service_base_dir();
    std::fs::create_dir_all(&base)
        .map_err(|e| ShellError::Message(format!("create {}: {e}", base.display())))?;

    let exe_dir = exe.parent().unwrap_or(Path::new("."));
    for name in ["routes.toml", "providers.toml"] {
        let target = base.join(name);
        if target.exists() {
            continue;
        }
        let source = exe_dir.join(name);
        if source.is_file() {
            std::fs::copy(&source, &target).map_err(|e| {
                ShellError::Message(format!("seed {name} from {}: {e}", source.display()))
            })?;
            println!("  seeded {name} from {}", source.display());
        }
    }

    let env_file = base.join("gateway.env");
    if !env_file.exists() {
        std::fs::write(&env_file, ENV_FILE_TEMPLATE)
            .map_err(|e| ShellError::Message(format!("write {}: {e}", env_file.display())))?;
        println!("  wrote gateway.env template");
    }
    Ok(base)
}

const ENV_FILE_TEMPLATE: &str = "\
# top-gateway service environment — one KEY=VALUE per line, '#' comments.
# This file takes the place of environment variables for the service.
# Admin key for /__admin/*: generate a real key and uncomment.
# GATEWAY_ADMIN_KEY=change-me
# The service usually listens on all interfaces, not loopback:
# GATEWAY_BIND=0.0.0.0:8000
# RUST_LOG=top_gateway=info
";

fn uninstall() -> Result<i32, ShellError> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    let service = open_service(
        &manager,
        ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    )?;
    match service.query_status() {
        Ok(status) if status.current_state != ServiceState::Stopped => {
            return Err(ShellError::Message(format!(
                "service is {:?} — stop it first: top-gateway stop",
                status.current_state
            )))
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("could not query service state: {e}"),
    }
    service.delete()?;
    println!("service `{SERVICE_NAME}` uninstalled (config files left in place)");
    Ok(0)
}

fn start() -> Result<i32, ShellError> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    let service = open_service(&manager, ServiceAccess::START)?;
    let no_args: [&str; 0] = [];
    if let Err(e) = service.start(&no_args) {
        if let windows_service::Error::Winapi(io) = &e {
            if io.raw_os_error() == Some(C_SERVICE_NOT_ACTIVE) {
                return Err(ShellError::Message(format!(
                    "service `{SERVICE_NAME}` is already running"
                )));
            }
        }
        return Err(e.into());
    }
    println!("service `{SERVICE_NAME}` started");
    Ok(0)
}

fn stop() -> Result<i32, ShellError> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    let service = open_service(&manager, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
    if let Err(e) = service.stop() {
        if let windows_service::Error::Winapi(io) = &e {
            if io.raw_os_error() == Some(C_SERVICE_NOT_ACTIVE) {
                println!("service `{SERVICE_NAME}` is not running");
                return Ok(0);
            }
        }
        return Err(e.into());
    }
    // `stop()` returns once the request is accepted; the service then drains
    // in-flight connections (graceful shutdown), so wait for the real stop.
    let deadline = Duration::from_secs(30);
    let mut waited = Duration::ZERO;
    loop {
        std::thread::sleep(Duration::from_millis(300));
        if service.query_status()?.current_state == ServiceState::Stopped {
            break;
        }
        waited += Duration::from_millis(300);
        if waited >= deadline {
            return Err(ShellError::Message(format!(
                "service did not report Stopped within {}s",
                deadline.as_secs()
            )));
        }
    }
    println!("service `{SERVICE_NAME}` stopped");
    Ok(0)
}

fn status() -> Result<i32, ShellError> {
    let manager = open_manager(ServiceManagerAccess::CONNECT)?;
    let service = open_service(&manager, ServiceAccess::QUERY_STATUS)?;
    let st = service.query_status()?;
    println!(
        "{}: {:?} (pid {:?}, exit code {:?})",
        SERVICE_NAME, st.current_state, st.process_id, st.exit_code
    );
    // Exit code for scripting: 0 = running, 1 = anything else.
    Ok(if st.current_state == ServiceState::Running {
        0
    } else {
        1
    })
}

// -------------------------------------------------------------------- service

/// The config directory the service uses: `GATEWAY_BASE_DIR` if set, else
/// `%ProgramData%\top-gateway`. A service's working directory is
/// `C:\Windows\System32` and it does not inherit the installer's environment,
/// so both config paths and `gateway.env` resolve here instead (ADR-S1).
fn service_base_dir() -> PathBuf {
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    config::base_dir_or(&PathBuf::from(program_data).join("top-gateway"))
}

// Generated low-level service entry; parses SCM arguments and calls
// service_main below.
define_windows_service!(ffi_service_main, service_main);

/// Service entry point, invoked on a background thread once the process is
/// connected to the SCM. Owns the whole service lifetime: status reporting,
/// logging setup, the tokio runtime, and the final Stopped report. Exits the
/// process when done — `service_dispatcher::start` unblocks afterwards.
fn service_main(_arguments: Vec<OsString>) {
    let code = match run_service() {
        Ok(code) => code,
        Err(e) => {
            // No console attached; log to file when possible, and let the
            // process exit code speak to the SCM's recovery actions.
            tracing::error!("top-gateway service failed: {e}");
            eprintln!("top-gateway service failed: {e}");
            1
        }
    };
    std::process::exit(code);
}

fn run_service() -> Result<i32, ShellError> {
    // The stop trigger: the control handler runs on an SCM thread, the server
    // on the tokio runtime — a oneshot holds the value even if Stop arrives
    // before the shutdown future is first polled.
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    // The handler is `FnMut` (it may fire more than once) while `oneshot::send`
    // is single-shot, so take the sender on the first Stop.
    let stop_tx = std::sync::Mutex::new(Some(stop_tx));

    let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
        match control {
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            ServiceControl::Stop => {
                if let Some(tx) = stop_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            // System shutdown is not accepted (controls_accepted below): the SCM
            // reaps the process on a short timer anyway, and draining proxied
            // requests would only delay machine shutdown.
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })?;

    report(
        &status_handle,
        ServiceState::StartPending,
        ServiceExitCode::Win32(0),
        Some(Duration::from_secs(10)),
    )?;

    // Base directory + gateway.env before settings, so every later warning
    // (missing admin key, missing config file) lands in the log file. `gateway.env`
    // may itself set GATEWAY_BASE_DIR, hence the re-resolution; the env file is
    // only read once, from the directory that won before it was applied.
    let base = service_base_dir();
    if let Err(e) = config::load_env_file(&base.join("gateway.env")) {
        eprintln!(
            "top-gateway: cannot read {}: {e} — continuing with the plain environment",
            base.join("gateway.env").display()
        );
    }
    let base = service_base_dir();
    std::env::set_var("GATEWAY_BASE_DIR", &base);

    let guard = match logging::init_file(&base) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!(
                "top-gateway: cannot open log file in {}: {e} — check directory permissions",
                base.display()
            );
            let _ = report(
                &status_handle,
                ServiceState::Stopped,
                ServiceExitCode::Win32(1),
                None,
            );
            return Ok(1);
        }
    };
    tracing::info!(
        "top-gateway service starting (config dir: {})",
        base.display()
    );

    report(
        &status_handle,
        ServiceState::Running,
        ServiceExitCode::Win32(0),
        None,
    )?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let exit = runtime.block_on(async move {
        let shutdown: BoxFuture<'static, ()> = Box::pin(async move {
            let _ = stop_rx.await;
        });
        match crate::run_with_shutdown(Settings::from_env(), shutdown).await {
            Ok(()) => 0,
            Err(e) => {
                tracing::error!("fatal: {e}");
                1
            }
        }
    });
    runtime.shutdown_timeout(Duration::from_secs(5));

    // Flush the log writer before reporting Stopped so the final messages
    // survive even if the SCM reaps the process promptly.
    drop(guard);

    report(
        &status_handle,
        ServiceState::Stopped,
        win32_exit(exit),
        None,
    )?;
    Ok(exit)
}

/// Gateway exit code (0/1) mapped to a service exit code.
fn win32_exit(code: i32) -> ServiceExitCode {
    ServiceExitCode::Win32(if code == 0 { 0 } else { 1 })
}

fn report(
    handle: &ServiceStatusHandle,
    state: ServiceState,
    exit_code: ServiceExitCode,
    wait_hint: Option<Duration>,
) -> Result<(), ShellError> {
    handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP
        } else {
            ServiceControlAccept::empty()
        },
        exit_code,
        checkpoint: 0,
        wait_hint: wait_hint.unwrap_or_default(),
        process_id: None,
    })?;
    Ok(())
}

// -------------------------------------------------------------------- console

/// `top-gateway run`: identical to the non-Windows build — stdout logging,
/// Ctrl+C to stop, config relative to the working directory.
fn run_console() -> Result<i32, ShellError> {
    logging::init_console();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result: Result<(), Box<dyn std::error::Error + Send + Sync>> =
        runtime.block_on(crate::run(Settings::from_env()));
    match result {
        Ok(()) => Ok(0),
        Err(e) => Err(ShellError::Message(e.to_string())),
    }
}
