//! A standalone Windows exe bootstraps the same supervisor used by upgrades.
//! No installed scripts, Rust, Python, VBScript, or manual setup is required.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

pub async fn run(
    action: &str,
    config: Option<&str>,
    connect: Option<&str>,
    name: Option<&str>,
) -> Result<()> {
    if action == "start" && crate::windows_upgrade::recover_interrupted()? {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let root = crate::windows_upgrade::installation_root(&exe)?;
    let explicit = config.is_some() || connect.is_some() || name.is_some();
    let saved = root.join(".portal-launch.json");
    let (launch, default_config) = if action != "start" || (!explicit && saved.is_file()) {
        (Value::Null, Value::Null)
    } else {
        make_launch(&root, config, connect, name)?
    };
    let stage = root
        .join(".portal-start")
        .join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&stage).context(
        "Portal needs a writable folder; move the exe to a folder owned by your Windows user",
    )?;
    crate::windows_upgrade::export_runtime(&stage.join("support"))?;
    std::fs::write(
        stage.join("portal-lifecycle.ps1"),
        include_str!("../../scripts/portal-lifecycle.ps1"),
    )?;
    std::fs::write(
        stage.join("portal-task-common.ps1"),
        include_str!("../../scripts/portal-task-common.ps1"),
    )?;
    let worker = stage.join("portal-start-worker.ps1");
    std::fs::write(
        &worker,
        include_str!("../../scripts/portal-start-worker.ps1"),
    )?;
    let request = json!({
        "action": action, "root": root, "target": exe, "launch": launch,
        "default_config": default_config, "explicit": explicit,
        "parent_pid": std::process::id(), "version": crate::upgrade::PORTAL_VERSION,
    });
    std::fs::write(stage.join("request.json"), serde_json::to_vec(&request)?)?;
    let powershell =
        PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is missing")?)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = tokio::process::Command::new(powershell);
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(worker)
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000 | 0x0000_0200)
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(120), command.output()).await;
    // These helper files are disposable; running supervisors use root/scripts.
    // Remove only the fixed files we created, never recurse through user data.
    for name in [
        "portal-lifecycle.ps1",
        "portal-supervisor.ps1",
        "portal-supervisor-bootstrap.ps1",
        "portal-supervisor-hidden.vbs",
    ] {
        let _ = std::fs::remove_file(stage.join("support").join(name));
    }
    let _ = std::fs::remove_dir(stage.join("support"));
    for name in [
        "portal-lifecycle.ps1",
        "portal-task-common.ps1",
        "portal-start-worker.ps1",
        "request.json",
    ] {
        let _ = std::fs::remove_file(stage.join(name));
    }
    let _ = std::fs::remove_dir(&stage);
    let output = result.context("Timed out starting Portal supervision; inspect portal-runtime.err.log and .portal-start-status.json")?
        .context("Windows PowerShell could not start the Portal supervisor")?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        bail!("Portal {action} failed: {}", detail.trim());
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn make_launch(
    root: &Path,
    config: Option<&str>,
    connect: Option<&str>,
    name: Option<&str>,
) -> Result<(Value, Value)> {
    let config_path = match config {
        Some(path) => std::path::absolute(path)?,
        None => root.join("portal.toml"),
    };
    let (resolved, default_config) = if config_path.is_file() {
        (
            crate::config::PortalConfig::load(
                config_path
                    .to_str()
                    .context("Config path must be Unicode")?,
            )?,
            Value::Null,
        )
    } else {
        anyhow::ensure!(
            config.is_none(),
            "Config file not found: {}",
            config_path.display()
        );
        let mut defaults = crate::config::PortalConfig::default();
        defaults.bind_host = "127.0.0.1".into();
        (
            defaults,
            Value::String(include_str!("../../portal.example.toml").to_string()),
        )
    };
    let connection = connect
        .map(str::to_owned)
        .or_else(|| {
            std::env::var("PORTAL_CONNECT_LINK")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            std::fs::read_to_string(root.join(".portal-connection.url"))
                .ok()
                .map(|value| value.trim().to_owned())
        });
    let identity = if let Some(link) = &connection {
        let (host, being, _) = crate::relay_client::parse_loom_link(link)?;
        format!("{}/{being}", host.to_ascii_lowercase())
    } else {
        format!("standalone/{}:{}", resolved.bind_host, resolved.bind_port)
    };
    let saved_name = std::fs::read_to_string(root.join(".portal-name")).ok();
    let portal_name = crate::relay_portal_name(
        name.map(str::to_owned)
            .or_else(|| saved_name.map(|value| value.trim().to_owned())),
        &resolved.name,
        crate::default_relay_portal_name,
    );
    let mut environment: BTreeMap<String, String> = [
        "PATH",
        "HOME",
        "USERPROFILE",
        "PORTAL_MCP_TOKEN",
        "RUST_LOG",
    ]
    .iter()
    .filter_map(|key| {
        std::env::var(key)
            .ok()
            .map(|value| (key.to_string(), value))
    })
    .collect();
    // The relay credential is passed through the child's environment, not argv.
    environment.insert("PORTAL_CONNECT_LINK".into(), connection.unwrap_or_default());
    Ok((
        json!({
            "protocol": 1, "identity": identity, "name": portal_name,
            "arguments": ["--config", config_path, "--name", portal_name],
            "working_directory": std::env::current_dir()?, "environment": environment,
        }),
        default_config,
    ))
}
