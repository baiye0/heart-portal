//! Heart Portal — Being's gateway to the world.
//! 
//! A lightweight MCP server with built-in tools (exec, file, web).
//! Heart's MCP supervisor connects to Portal via TCP.
//! Portal can run on Town Home, a human's laptop, or anywhere.

mod config;
mod exec_policy;
mod process_manager;
mod tools;
mod kits;
mod mcp;
mod protocol;
mod relay_client;
mod single_instance;
mod upgrade;
#[cfg(target_os = "macos")]
mod macos_upgrade;
#[cfg(target_os = "macos")]
mod macos_supervisor;
#[cfg(windows)]
mod windows_upgrade;
#[cfg(windows)]
mod windows_start;
#[cfg(windows)]
mod connection_status;

use std::path::PathBuf;
use std::time::Duration;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tracing::{info, warn, debug, trace};

use crate::config::PortalConfig;
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, JsonRpcError, PORTAL_VERSION};
use crate::tools::ToolHost;

#[derive(Parser)]
#[command(
    name = "heart-portal",
    version = PORTAL_VERSION,
    about = "Heart Portal — Being's gateway to the world"
)]
struct Cli {
    /// Export version-matched Windows supervision code for the update worker
    #[arg(long, hide = true)]
    export_windows_runtime: Option<PathBuf>,
    /// Legacy spelling for the upgrade subcommand
    #[arg(long = "upgrade", hide = true)]
    legacy_upgrade: bool,
    #[command(subcommand)]
    command: Option<Commands>,

    /// Loom URL for reverse relay (Home mode)
    #[arg(long)]
    connect: Option<String>,

    /// Portal name for relay handshake identity
    #[arg(long)]
    name: Option<String>,

    /// Path to portal.toml
    #[arg(short = 'c', long = "config")]
    config: Option<String>,

    /// Path to portal.toml (positional)
    #[arg(value_name = "CONFIG")]
    config_positional: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Stop this Windows/macOS Portal and its supervision
    Stop,
    /// Show the running Windows/macOS Portal and supervisor status
    Status,
    /// Check GitHub releases and upgrade to the latest version
    Upgrade {
        /// Apply a pre-downloaded newer Windows/macOS binary through the same updater
        #[arg(long, conflicts_with = "status")]
        file: Option<PathBuf>,
        /// Show the last Windows/macOS upgrade transaction without downloading
        #[arg(long)]
        status: bool,
        /// Migrate an existing macOS installation using this downloaded new executable
        #[arg(long, conflicts_with_all = ["file", "status"])]
        target: Option<PathBuf>,
    },
    /// Manage installed Portal kits
    Kit {
        #[command(subcommand)]
        command: KitCommands,
    },
}

#[derive(Subcommand)]
enum KitCommands {
    /// List installed kits
    List,
    /// Show kit pre-flight status
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(path) = &cli.export_windows_runtime {
        #[cfg(windows)]
        return windows_upgrade::export_runtime(path);
        #[cfg(not(windows))]
        anyhow::bail!("Windows runtime export is available only on Windows");
    }
    let command = cli.command;

    if matches!(&command, Some(Commands::Upgrade { status: true, .. })) {
        #[cfg(windows)]
        return windows_upgrade::show_status();
        #[cfg(target_os = "macos")]
        return macos_upgrade::show_status();
        #[cfg(not(any(windows, target_os = "macos")))]
        anyhow::bail!("Upgrade status is supported on Windows and macOS");
    }
    if let Some(Commands::Upgrade { file: Some(path), .. }) = &command {
        #[cfg(windows)]
        return windows_upgrade::upgrade_file(path).await;
        #[cfg(target_os = "macos")]
        return macos_upgrade::handoff(&tokio::fs::read(path).await?, None).await;
        #[cfg(not(any(windows, target_os = "macos")))]
        anyhow::bail!("Local executable upgrades are supported on Windows and macOS");
    }
    if let Some(Commands::Upgrade { target: Some(path), .. }) = &command {
        #[cfg(target_os = "macos")]
        return macos_upgrade::migrate(path).await;
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            anyhow::bail!("Installation migration is supported on macOS");
        }
    }
    if cli.legacy_upgrade || matches!(&command, Some(Commands::Upgrade { .. })) {
        return upgrade::run_upgrade().await;
    }
    if matches!(&command, Some(Commands::Stop | Commands::Status)) {
        #[cfg(windows)]
        return windows_start::run(if matches!(&command, Some(Commands::Stop)) { "stop" } else { "status" }, None, None, None).await;
        #[cfg(target_os = "macos")]
        return macos_supervisor::action(if matches!(&command, Some(Commands::Stop)) { "stop" } else { "status" }).await;
        #[cfg(not(any(windows, target_os = "macos")))]
        anyhow::bail!("Use your OS service manager for start/stop/status on this platform");
    }
    #[cfg(windows)]
    if command.is_none() && std::env::var("HEART_PORTAL_SUPERVISED").as_deref() != Ok("1") {
        return windows_start::run("start", cli.config.as_deref().or(cli.config_positional.as_deref()), cli.connect.as_deref(), cli.name.as_deref()).await;
    }
    #[cfg(target_os = "macos")]
    if command.is_none() {
        macos_upgrade::recover_interrupted()?;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,heart_portal=debug".parse().unwrap_or_else(|e| {
                    eprintln!("Failed to parse default log filter: {}", e);
                    tracing_subscriber::EnvFilter::new("info")
                }))
        )
        .init();

    // Supervisors can provide the Loom link through the environment so the
    // credential is not exposed in the OS process command line.
    let connect_link = cli.connect.or_else(|| {
        std::env::var("PORTAL_CONNECT_LINK")
            .ok()
            .filter(|link| !link.trim().is_empty())
    });
    let explicit_config = cli.config.is_some() || cli.config_positional.is_some();
    let config_path = cli
        .config
        .or(cli.config_positional)
        .unwrap_or_else(|| "portal.toml".to_string());
    let cli_portal_name = cli.name;
    
    let mut config = if PathBuf::from(&config_path).exists() {
        PortalConfig::load(&config_path)?
    } else {
        anyhow::ensure!(!explicit_config, "Config file not found: {}", config_path);
        info!("No config file at {}, using defaults", config_path);
        PortalConfig::default()
    };

    if let Ok(t) = std::env::var("PORTAL_MCP_TOKEN") {
        if !t.is_empty() {
            config.portal_mcp_token = Some(t);
        }
    }

    if let Some(Commands::Kit { command }) = &command {
        return match command {
            KitCommands::List => list_installed_kits(&config).await,
            KitCommands::Status => show_kit_status(&config).await,
        };
    }

    // Prevent stale/duplicate Portal processes from competing for the
    // same relay connection and ejecting one another. Management subcommands
    // above remain usable while a Portal instance is running.
    // Key the instance guard by relay host + Being rather than by the whole
    // Loom URL. Rotating a token must not allow a second local instance to
    // bypass the duplicate-process guard.
    #[cfg(windows)]
    if windows_upgrade::recover_interrupted()? { return Ok(()); }
    #[cfg(windows)]
    let startup_guard = windows_upgrade::startup_guard()?;
    #[cfg(target_os = "macos")]
    let startup_guard = macos_upgrade::startup_guard()?;
    let instance_identity = match connect_link.as_deref() {
        Some(link) => {
            let (host, being_id, _) = relay_client::parse_loom_link(link)?;
            format!("{}/{being_id}", host.to_ascii_lowercase())
        }
        None => format!("standalone/{}:{}", config.bind_host, config.bind_port),
    };
    let _single_instance =
        single_instance::acquire(Some(&instance_identity)).map_err(|e| anyhow::anyhow!(e))?;

    config.prepare_workspace()?;
    info!("Workspace ready: {}", config.security.workspace_root.display());

    #[cfg(target_os = "macos")]
    macos_supervisor::start(&config_path, connect_link.as_deref(), cli_portal_name.as_deref()).await?;

    if config.portal_mcp_token.is_none() {
        warn!("PORTAL_MCP_TOKEN is not set — MCP TCP connections are unauthenticated (set token for public deployments)");
    }

    if connect_link.is_some() {
        info!(
            "Portal '{}' connect mode — MCP via Hearth relay",
            config.name
        );
    } else {
        info!("Portal '{}' starting on {}:{}", config.name, config.bind_host, config.bind_port);
    }

    // Initialize tool host (built-in + custom)
    let tool_host = ToolHost::new(&config);

    if config.kits_enabled {
        tool_host.start_kit_refresh_task();
        info!("Kit manifest hot-reload started (every 60s)");
    }

    let cleanup_host = tool_host.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            cleanup_host.cleanup_background_sessions().await;
        }
    });

    // Load custom tools from workspace/tools/mcp.toml
    match tool_host.load_custom_tools().await {
        Ok(0) => info!("No custom tools loaded"),
        Ok(n) => info!("Loaded {} custom tools", n),
        Err(e) => warn!("Failed to load custom tools: {}", e),
    }

    // Pre-spawn eager kits (manifest.eager == true) so the first call has
    // no cold-start latency. Failures are logged, not fatal.
    tool_host.warmup_kits().await;

    let tool_list = tool_host.list_tools().await;
    info!("Portal tools: {}", tool_list.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "));

    if let Some(ref loom) = connect_link {
        // Relay handshake identity: --name, non-generic config name, then host name.
        let relay_portal_name =
            relay_portal_name(cli_portal_name, &config.name, default_relay_portal_name);

        // Async callback: finished background sessions POST back to the being's
        // Heart, which writes the inbox and triggers breathe_callback.
        match relay_client::parse_loom_link(loom) {
            Ok((host, being_id, token)) => {
                let url = callback_url(loom, &host, &being_id);
                tool_host
                    .process_manager
                    .set_callback_config(url, token, relay_portal_name.clone());
            }
            Err(e) => warn!("async callback disabled (invalid Loom link): {e:#}"),
        }

        publish_supervisor_ready()?;
        #[cfg(any(windows, target_os = "macos"))]
        drop(startup_guard);
        let tool_shutdown = tool_host.clone();
        let restart_waiter = tool_host.clone();
        tokio::select! {
            _ = async {
                let _ = tokio::signal::ctrl_c().await;
            } => {
                info!("Portal shutting down (Ctrl+C)");
                #[cfg(target_os = "macos")]
                macos_supervisor::stop_on_interrupt();
                tool_shutdown.kill_all_managed_processes().await;
            }
            _ = wait_sigterm() => {
                info!("Portal shutting down (termination signal)");
                tool_shutdown.kill_all_managed_processes().await;
            }
            _ = restart_waiter.wait_for_restart() => {
                info!("Portal restarting after a controlled tool request");
                tool_shutdown.kill_all_managed_processes().await;
            }
            _ = relay_client::connect_and_serve(loom, &tool_host, &relay_portal_name) => {}
        }
        return Ok(());
    }

    // Track active connections
    let active_connections = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    // Listen for MCP supervisor connections
    let addr = format!("{}:{}", config.bind_host, config.bind_port);
    let listener = TcpListener::bind(&addr).await?;
    info!("Portal MCP listening on {}", addr);
    #[cfg(windows)]
    connection_status::publish("local");
    publish_supervisor_ready()?;
    #[cfg(any(windows, target_os = "macos"))]
    drop(startup_guard);

    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let mut shutdown_rx = shutdown_tx.subscribe();
    let shutdown_cleanup = tool_host.clone();
    let restart_waiter = tool_host.clone();
    tokio::spawn({
        let shutdown_tx = shutdown_tx.clone();
        async move {
            tokio::select! {
                r = tokio::signal::ctrl_c() => {
                    let _ = r;
                    #[cfg(target_os = "macos")]
                    macos_supervisor::stop_on_interrupt();
                }
                _ = wait_sigterm() => {}
                _ = restart_waiter.wait_for_restart() => {
                    info!("Portal restarting after a controlled tool request");
                }
            }
            info!("Portal shutting down");
            let _ = shutdown_tx.send(());
        }
    });
    drop(shutdown_tx);

    loop {
        tokio::select! {
            biased;
            res = shutdown_rx.recv() => {
                match res {
                    Ok(()) | Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
                shutdown_cleanup.kill_all_managed_processes().await;
                break;
            }
            accept = listener.accept() => {
                let (stream, peer) = accept.context("MCP listener accept failed")?;
                let conn_count = active_connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                info!("MCP client connected from {} (active: {})", peer, conn_count);

                let tool_host = tool_host.clone();
                let portal_name = config.name.clone();
                let mcp_token = config.portal_mcp_token.clone();
                let active = active_connections.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &tool_host, &portal_name, mcp_token.as_deref()).await {
                        warn!("Connection from {} ended: {}", peer, e);
                    } else {
                        info!("Connection from {} closed cleanly", peer);
                    }
                    let remaining = active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) - 1;
                    info!("Connection closed (active: {})", remaining);
                });
            }
        }
    }

    drop(listener);
    Ok(())
}

/// Local readiness is independent of relay availability: a network outage must
/// not turn a working binary into a failed upgrade. The supervisor checks the
/// PID, a fresh per-launch nonce, and the process creation time as well.
fn publish_supervisor_ready() -> Result<()> {
    #[cfg(windows)]
    windows_upgrade::publish_ready()?;
    #[cfg(target_os = "macos")]
    macos_upgrade::publish_ready()?;
    Ok(())
}

async fn list_installed_kits(config: &PortalConfig) -> Result<()> {
    if !config.kits_enabled {
        println!("Kits disabled");
        return Ok(());
    }

    let kits_dir = kits::loader::kits_dir(config);
    let kits = kits::loader::load_kits(config)?;
    if kits.is_empty() {
        println!("No kits installed in {}", kits_dir.display());
        return Ok(());
    }

    let manager = kits::manager::KitManager::new(kits);
    println!("Installed kits in {}", kits_dir.display());
    for status in manager.statuses().await {
        println!(
            "{}\t{}\t{}\t{} tool(s)",
            status.name, status.version, status.status, status.tools
        );
    }

    Ok(())
}

async fn show_kit_status(config: &PortalConfig) -> Result<()> {
    if !config.kits_enabled {
        println!("Kits disabled");
        return Ok(());
    }

    let kits_dir = kits::loader::kits_dir(config);
    let kits = kits::loader::load_kits_from_dir(&kits_dir)?;
    if kits.is_empty() {
        println!("No kits installed in {}", kits_dir.display());
        return Ok(());
    }

    println!(
        "{:<16} {:<8} {:<6} {:<11} {}",
        "Kit", "Version", "Tools", "Status", "Command"
    );
    for kit in kits {
        let status = if kits::loader::command_binary_exists(&kit.command) {
            "not-started"
        } else {
            "unhealthy"
        };
        println!(
            "{:<16} {:<8} {:<6} {:<11} {}",
            &kit.manifest.name,
            &kit.manifest.version,
            kit.manifest.tools.len(),
            status,
            kits::loader::format_command(&kit.command)
        );
    }

    Ok(())
}

/// Derive Heart's callback endpoint from the Loom link.
/// `https://echo.beings.town/alice/?token=…` → `https://echo.beings.town/alice/api/callback`.
/// Scheme follows the Loom link, except localhost/127.* which is always plain http.
fn callback_url(loom_link: &str, host: &str, being_id: &str) -> String {
    let is_localhost = host.starts_with("localhost") || host.starts_with("127.");
    let scheme = if is_localhost {
        "http"
    } else if loom_link.trim_start().starts_with("http://") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}/{being_id}/api/callback")
}

fn default_relay_portal_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "portal".to_string())
}

fn relay_portal_name(
    cli_portal_name: Option<String>,
    config_name: &str,
    fallback: impl FnOnce() -> String,
) -> String {
    cli_portal_name
        .or_else(|| {
            if !config_name.is_empty() && config_name != "portal" {
                Some(config_name.to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(fallback)
}

#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            warn!("Failed to install SIGTERM handler: {}; falling back to Ctrl+C", e);
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(windows)]
async fn wait_sigterm() {
    match tokio::signal::windows::ctrl_break() {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            warn!("Failed to install CTRL_BREAK handler: {}; falling back to Ctrl+C", e);
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(any(unix, windows)))]
async fn wait_sigterm() {
    let _ = tokio::signal::ctrl_c().await;
}

/// Handle a single MCP client connection (JSON-RPC over newline-delimited TCP)
pub(crate) async fn handle_connection<S>(
    stream: S,
    tool_host: &ToolHost,
    portal_name: &str,
    expected_token: Option<&str>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);
    let mut line = String::new();

    if let Some(expected) = expected_token.filter(|t| !t.is_empty()) {
        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                debug!("Client disconnected before auth (EOF)");
                return Ok(());
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            debug!("← auth line: {}", trimmed);

            let value: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    let error_resp = JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: None,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32700,
                            message: format!("Parse error: {}", e),
                            data: None,
                        }),
                    };
                    send_response(&mut writer, &error_resp).await?;
                    anyhow::bail!("MCP auth: invalid JSON");
                }
            };

            let method = value.get("method").and_then(|v| v.as_str()).unwrap_or_else(|| {
                debug!("Missing or invalid 'method' field in JSON-RPC request");
                ""
            });
            let id = value.get("id").cloned();
            if method != "auth" {
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.as_ref().and_then(|v| v.as_u64()),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32001,
                        message: "Unauthorized: first message must be {\"method\":\"auth\",\"params\":{\"token\":\"...\"}}"
                            .to_string(),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                anyhow::bail!("MCP auth: expected auth as first message");
            }

            let token = value
                .get("params")
                .and_then(|p| p.get("token"))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    debug!("Missing or invalid token in auth params");
                    ""
                });
            if token != expected {
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.as_ref().and_then(|v| v.as_u64()),
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32002,
                        message: "Unauthorized: invalid token".to_string(),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                anyhow::bail!("MCP auth: invalid token");
            }

            let ok = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: id.as_ref().and_then(|v| v.as_u64()),
                result: Some(serde_json::json!({ "authenticated": true })),
                error: None,
            };
            send_response(&mut writer, &ok).await?;
            break;
        }
    }

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            debug!("Client disconnected (EOF)");
            return Ok(());
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        debug!("← {}", trimmed);

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                warn!("Invalid JSON-RPC: {}", e);
                let error_resp = JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: None,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                };
                send_response(&mut writer, &error_resp).await?;
                continue;
            }
        };

        // Notifications (no id) — just ack
        if request.id.is_none() {
            debug!("Notification: {}", request.method);
            continue;
        }

        let response = handle_request(&request, tool_host, portal_name).await;
        send_response(&mut writer, &response).await?;
        if request.method == "tools/call"
            && request.params.get("name").and_then(|v| v.as_str()) == Some("portal_restart")
        {
            tool_host.restart_after_response();
        }

        // After tool reload, send MCP notification instead of closing connection
        if tool_host.needs_reconnect.load(std::sync::atomic::Ordering::SeqCst) {
            tool_host.needs_reconnect.store(false, std::sync::atomic::Ordering::SeqCst);
            info!("🔄 Sending notifications/tools/list_changed after tools reload");
            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
                "params": {}
            });
            if let Err(e) = send_notification(&mut writer, &notification).await {
                warn!("Failed to send tools/list_changed notification: {e}, closing connection");
                return Ok(());
            }
        }
    }
}

/// Route a JSON-RPC request to the appropriate handler
async fn handle_request(
    request: &JsonRpcRequest,
    tool_host: &ToolHost,
    portal_name: &str,
) -> JsonRpcResponse {
    let id = request.id;

    match request.method.as_str() {
        "initialize" => {
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": { "listChanged": false }
                    },
                    "serverInfo": {
                        "name": format!("heart-portal-{}", portal_name),
                        "version": PORTAL_VERSION
                    }
                })),
                error: None,
            }
        }

        "tools/list" => {
            let tools: Vec<serde_json::Value> = tool_host.list_tools().await.iter().map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": t.input_schema
                })
            }).collect();

            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({ "tools": tools })),
                error: None,
            }
        }

        "tools/call" => {
            let tool_name = request.params.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    debug!("Missing or invalid tool name in tools/call request");
                    ""
                });
            let arguments = request.params.get("arguments")
                .cloned()
                .and_then(|v| if v.is_object() { Some(v) } else { None })
                .unwrap_or_else(|| {
                    debug!("Missing or invalid arguments in tools/call request, using empty object");
                    serde_json::json!({})
                });

            let start = std::time::Instant::now();
            info!("⚡ {} called", tool_name);

            let result = tool_host.call(tool_name, arguments).await;
            let elapsed = start.elapsed();

            match result {
                Ok(value) => {
                    let is_error = value.get("isError").and_then(|v| v.as_bool()).unwrap_or_else(|| {
                        if value.get("content").is_none() {
                            warn!("Tool '{}' returned a malformed result with no content and no valid isError field; assuming success", tool_name);
                        } else {
                            trace!("Missing or invalid isError field in tool result, assuming success");
                        }
                        false
                    });
                    if is_error {
                        warn!("⚡ {} → error ({:?})", tool_name, elapsed);
                    } else {
                        info!("⚡ {} → ok ({:?})", tool_name, elapsed);
                    }
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(value),
                        error: None,
                    }
                }
                Err(e) => {
                    warn!("⚡ {} → fail: {} ({:?})", tool_name, e, elapsed);
                    let message = format!("Tool error: {}", e);
                    JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(serde_json::json!({
                            "content": [{"type": "text", "text": message}],
                            "isError": true
                        })),
                        error: None,
                    }
                }
            }
        }

        "ping" => {
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(serde_json::json!({})),
                error: None,
            }
        }

        _ => {
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: format!("Method not found: {}", request.method),
                    data: None,
                }),
            }
        }
    }
}

/// Send a JSON-RPC response (newline-delimited)
async fn send_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    response: &JsonRpcResponse,
) -> Result<()> {
    let json = serde_json::to_string(response)?;
    debug!("→ {}", json);
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// Send a JSON-RPC notification (no id, no response expected).
async fn send_notification<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut BufWriter<W>,
    notification: &serde_json::Value,
) -> Result<()> {
    let json = serde_json::to_string(notification)?;
    debug!("→ (notification) {}", json);
    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_portal_name_prefers_cli_name() {
        let name = relay_portal_name(Some("foo".to_string()), "cotton", || "host".to_string());
        assert_eq!(name, "foo");
    }

    #[test]
    fn relay_portal_name_uses_non_generic_config_name() {
        let name = relay_portal_name(None, "cotton", || "host".to_string());
        assert_eq!(name, "cotton");
    }

    #[test]
    fn relay_portal_name_skips_generic_config_name() {
        let name = relay_portal_name(None, "portal", || "host".to_string());
        assert_eq!(name, "host");
    }

    #[test]
    fn relay_portal_name_skips_empty_config_name() {
        let name = relay_portal_name(None, "", || "host".to_string());
        assert_eq!(name, "host");
    }

    #[test]
    fn callback_url_follows_loom_scheme() {
        assert_eq!(
            callback_url(
                "https://echo.beings.town/alice/?token=abc",
                "echo.beings.town",
                "alice"
            ),
            "https://echo.beings.town/alice/api/callback"
        );
    }

    #[test]
    fn callback_url_uses_http_for_localhost() {
        assert_eq!(
            callback_url("https://localhost:3100/hex/?token=abc", "localhost:3100", "hex"),
            "http://localhost:3100/hex/api/callback"
        );
        assert_eq!(
            callback_url("http://127.0.0.1:3100/hex/?token=abc", "127.0.0.1:3100", "hex"),
            "http://127.0.0.1:3100/hex/api/callback"
        );
    }

    #[test]
    fn callback_url_keeps_plain_http_for_remote_http_loom() {
        assert_eq!(
            callback_url("http://box.local:8080/bee/?token=abc", "box.local:8080", "bee"),
            "http://box.local:8080/bee/api/callback"
        );
    }

    #[test]
    fn callback_url_never_carries_the_token() {
        let url = callback_url(
            "https://echo.beings.town/alice/?token=supersecret",
            "echo.beings.town",
            "alice",
        );
        assert!(!url.contains("supersecret"));
        assert!(!url.contains('?'));
    }
}
