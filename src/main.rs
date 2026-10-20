use ai_router::{
    antigravity::Antigravity,
    api::{self, AppState},
    auth,
    config::Config,
    monitor::{self, MonitorArgs, Telemetry},
};
use std::future::IntoFuture;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("ai-router: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("serve") {
        "openai" => return ai_router::openai_auth::operator(&args[1..]).await,
        "antigravity" => return antigravity_operator(&args[1..]).await,
        "keygen" => {
            let id = args
                .get(1)
                .ok_or("Usage: ai-router keygen <client-id> [model]")?;
            if id.is_empty()
                || id.len() > 64
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err("Invalid client ID".into());
            }
            let scopes = if args.len() > 2 {
                args[2..].to_vec()
            } else {
                vec!["model".into()]
            };
            if scopes.iter().any(|s| s != "model") {
                return Err("The only supported scope is model".into());
            }
            let (key, record) = auth::generate_key(id, scopes);
            ai_router::config::validate_key(&record)?;
            // Raw key is shown only to the operator, once; configuration contains its digest.
            eprintln!("Save this API key in the client: {key}");
            println!(
                "{}",
                serde_json::to_string(&record).map_err(|_| "Could not encode key record")?
            );
            return Ok(());
        }
        "monitor" => {
            let mut options = MonitorArgs {
                json: false,
                text: false,
                range_seconds: 86400,
            };
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--json" => options.json = true,
                    "--text" => options.text = true,
                    "--range" => {
                        i += 1;
                        options.range_seconds =
                            parse_range(args.get(i).ok_or("Missing monitor range")?)?;
                    }
                    _ => {
                        return Err(
                            "Usage: ai-router monitor [--json|--text] [--range 24h|1h|seconds]"
                                .into(),
                        );
                    }
                }
                i += 1;
            }
            let socket = PathBuf::from(
                std::env::var("AI_ROUTER_MONITOR_SOCKET")
                    .unwrap_or_else(|_| "/run/ai-router/monitor.sock".into()),
            );
            return monitor::client(&socket, options).await;
        }
        "serve" => {
            if args.len() > 1 {
                return Err("Usage: ai-router serve".into());
            }
        }
        "--help" | "-h" => {
            println!(
                "ai-router serve | openai login|logout|status|accounts|select|resume | keygen <id> [model] | monitor [--json|--text] [--range 24h] | antigravity login|status|logout|resume"
            );
            return Ok(());
        }
        _ => return Err("Unknown command; use --help".into()),
    }
    let config = Arc::new(Config::from_env()?);
    let telemetry = Arc::new(Telemetry::open(
        &config.telemetry_dir,
        config.retention_days,
        config.max_events,
    )?);
    let antigravity = Antigravity::new(config.clone());
    let state = AppState::new(config.clone(), antigravity, telemetry.clone());
    let cancel = CancellationToken::new();
    let monitor_cancel = cancel.clone();
    let socket = config.monitor_socket.clone();
    let monitor_task =
        tokio::spawn(async move { monitor::serve(telemetry, socket, monitor_cancel).await });
    let background = state.clone();
    let background_cancel = cancel.clone();
    let background_task = tokio::spawn(async move {
        let mut retry = 5;
        loop {
            background.refresh_agy().await;
            let healthy = background.provider.read().await.authenticated;
            let delay = probe_delay(healthy, &mut retry);
            tokio::select! {_ = background_cancel.cancelled()=>break,_ = tokio::time::sleep(delay)=>{}}
        }
    });
    // A stalled provider cannot delay publication or recovery of the other.
    let openai_background = state.clone();
    let openai_cancel = cancel.clone();
    let openai_background_task = tokio::spawn(async move {
        if !openai_background.config.openai_enabled {
            return;
        }
        let mut retry = 5;
        loop {
            openai_background.refresh_openai().await;
            let healthy = openai_background.openai_provider.read().await.authenticated;
            let delay = probe_delay(healthy, &mut retry);
            tokio::select! {_ = openai_cancel.cancelled()=>break,_ = tokio::time::sleep(delay)=>{}}
        }
    });
    // Keep monitor status aligned with request admission as cached account
    // availability can change between provider catalogue probes.
    let monitor_status = state.clone();
    let monitor_status_cancel = cancel.clone();
    let monitor_status_task = tokio::spawn(async move {
        loop {
            tokio::join!(
                monitor_status.publish_openai_monitor_status(),
                monitor_status.publish_agy_monitor_status()
            );
            tokio::select! {
                _ = monitor_status_cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    });
    let release = state.antigravity.release.clone();
    let release_telemetry = state.telemetry.clone();
    let release_cancel = cancel.clone();
    let release_task = tokio::spawn(async move {
        loop {
            tokio::select! { _=release_cancel.cancelled()=>break, value=release.refresh()=>release_telemetry.set_antigravity_release(value).await }
            tokio::select! { _=release_cancel.cancelled()=>break, _=tokio::time::sleep(Duration::from_secs(3600))=>{} }
        }
    });
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .map_err(|e| format!("Cannot bind HTTP listener: {e}"))?;
    eprintln!("ai-router listening on {}", config.bind);
    let shutdown_cancel = cancel.clone();
    let shutdown_state = state.clone();
    let shutdown_telemetry = state.telemetry.clone();
    let server = axum::serve(
        listener,
        api::router(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        #[cfg(unix)]
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("termination signal");
        #[cfg(unix)]
        tokio::select! {_ = tokio::signal::ctrl_c()=>{},_ = term.recv()=>{}}
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        shutdown_cancel.cancel();
        shutdown_state.stop();
    })
    .into_future();
    let mut server = Box::pin(server);
    let server_error = tokio::select! {
        result=&mut server=>result.err().map(|e|format!("HTTP listener failed: {e}")),
        _=cancel.cancelled()=>{match tokio::time::timeout(Duration::from_secs(8),&mut server).await {
            Ok(Err(error))=>Some(format!("HTTP listener failed: {error}")), _=>None,
        }},
    };
    drop(server);
    cancel.cancel();
    state.stop();
    let drained = state.drain(Duration::from_secs(6)).await;
    background_task.abort();
    openai_background_task.abort();
    let _ = release_task.await;
    let _ = monitor_status_task.await;
    let _ = monitor_task.await;
    shutdown_telemetry.flush().await;
    if drained {
        match server_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    } else {
        Err("Shutdown cleanup failed or exceeded its deadline; some client bodies or provider cleanup did not drain".into())
    }
}

fn parse_range(value: &str) -> Result<u64, String> {
    let (number, multiplier) = if let Some(v) = value.strip_suffix('h') {
        (v, 3600)
    } else if let Some(v) = value.strip_suffix('m') {
        (v, 60)
    } else if let Some(v) = value.strip_suffix('d') {
        (v, 86400)
    } else {
        (value, 1)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(multiplier))
        .filter(|n| *n > 0 && *n <= 30 * 86400)
        .ok_or_else(|| "Invalid monitor range (1 second to 30 days)".into())
}

fn probe_delay(healthy: bool, retry: &mut u64) -> Duration {
    if healthy {
        *retry = 5;
        return Duration::from_secs(60);
    }
    let delay = (*retry + u64::from(rand::random::<u8>() % 3)).min(60);
    *retry = retry.saturating_mul(2).min(60);
    Duration::from_secs(delay)
}

async fn antigravity_operator(args: &[String]) -> Result<(), String> {
    use ai_router::antigravity_auth::{AntigravityAuth, OAuthConfig};
    let directory = std::env::var("AI_ROUTER_STATE_DIR")
        .unwrap_or_else(|_| "/var/lib/ai-router/antigravity-auth".into());
    let auth = AntigravityAuth::new(directory.into(), OAuthConfig::from_env())
        .map_err(|e| e.to_string())?;
    let value = match args.first().map(String::as_str) {
        Some("login") => {
            let port = match args {
                [_] => 1456,
                [_, flag, value] if flag == "--port" => {
                    value.parse::<u16>().map_err(|_| "Invalid callback port")?
                }
                _ => return Err("Usage: ai-router antigravity login [--port N]".into()),
            };
            auth.login(port).await.map_err(|e| e.to_string())?
        }
        Some("status") if args.len() == 1 => auth.status().await.map_err(|e| e.to_string())?,
        Some("logout") if args.len() == 1 => {
            auth.logout().await.map_err(|e| e.to_string())?;
            auth.status().await.map_err(|e| e.to_string())?
        }
        Some("resume") if args.len() == 1 => {
            let generation = auth.generation().await.map_err(|e| e.to_string())?;
            auth.resume(&generation).await.map_err(|e| e.to_string())?;
            auth.status().await.map_err(|e| e.to_string())?
        }
        _ => {
            return Err(
                "Usage: ai-router antigravity login [--port N] | status | logout | resume".into(),
            );
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|_| "Could not format status")?
    );
    Ok(())
}
