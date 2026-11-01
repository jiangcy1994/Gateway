#[cfg(not(windows))]
fn main() {
    top_gateway::logging::init_console();
    let settings = top_gateway::config::Settings::from_env();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    if let Err(e) = rt.block_on(top_gateway::run(settings)) {
        eprintln!("top-gateway: {e}");
        std::process::exit(1);
    }
}

// One binary, two faces on Windows: the SCM launches it without arguments to
// run as a service; `run / install / start / stop / uninstall / status` are the
// management console. See `src/service.rs`.
#[cfg(windows)]
fn main() {
    let code = match top_gateway::service::dispatch() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("top-gateway: {e}");
            1
        }
    };
    std::process::exit(code);
}
