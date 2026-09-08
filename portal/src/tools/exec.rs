//! Exec tool — run shell commands.

use crate::config::PortalConfig;
use crate::exec_policy::{
    configure_shell_command, validate_exec_allowlist, validate_shell_command, ExecShell,
};
use crate::process_manager::ProcessManager;
use crate::tools::text::OutputEncoding;
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;
use tokio::process::Command;
use tracing::debug;

pub async fn execute(
    config: &PortalConfig,
    process_manager: &Arc<ProcessManager>,
    arguments: Value,
) -> Result<Value> {
    let command = arguments
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing 'command' argument"))?;
    let shell = ExecShell::parse(arguments.get("shell"))?;
    validate_shell_command(shell, command)?;
    let output_encoding = OutputEncoding::parse(arguments.get("output_encoding"))?.for_shell(shell);

    let workdir = arguments
        .get("workdir")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.security.workspace_root.to_string_lossy().to_string());

    let timeout_secs = arguments
        .get("timeout_secs")
        .and_then(super::value_as_u64)
        .unwrap_or_else(|| {
            debug!("Missing timeout_secs argument, using default 30 seconds");
            30
        })
        .min(300);

    let background = arguments
        .get("background")
        .and_then(super::value_as_bool)
        .unwrap_or_else(|| {
            debug!("Missing background argument, using synchronous execution");
            false
        });

    validate_exec_allowlist(command, &config.security.exec_allowlist)?;

    if background {
        let info = process_manager
            .spawn_with_shell(config, command, &workdir, &[], shell, output_encoding)
            .await?;
        return Ok(serde_json::json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&serde_json::json!({
                    "session_id": info.session_id,
                    "pid": info.pid,
                    "status": "running",
                    "output_encoding": info.output_encoding.as_str()
                }))?
            }],
            "isError": false
        }));
    }

    debug!(
        "exec: {} (workdir: {}, timeout: {}s)",
        command, workdir, timeout_secs
    );

    let mut cmd = Command::new(shell.program());
    configure_shell_command(&mut cmd, command, config, &workdir, shell);

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        cmd.output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Command timed out after {}s", timeout_secs))?
    .map_err(|e| anyhow::anyhow!("Failed to execute: {}", e))?;

    let stdout = output_encoding.decode(&output.stdout);
    let stderr = output_encoding.decode(&output.stderr);
    let exit_code = output.status.code().unwrap_or_else(|| {
        debug!("Process terminated by signal, no exit code available");
        -1
    });

    let mut text = format!(
        "{}{}",
        stdout,
        if !stderr.is_empty() {
            format!("\n--- stderr ---\n{}", stderr)
        } else {
            String::new()
        },
    );
    if exit_code != 0 {
        text.push_str(&format!("\n(exit code: {})", exit_code));
    }

    // Truncate large outputs to avoid flooding the being's context
    const MAX_OUTPUT_BYTES: usize = 100_000;
    let truncated = text.len() > MAX_OUTPUT_BYTES;
    if truncated {
        let end = super::text::byte_prefix(&text, MAX_OUTPUT_BYTES).len();
        text.truncate(end);
        text.push_str(&format!(
            "\n...\n(output truncated at {} bytes)",
            end
        ));
    }

    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": exit_code != 0,
        "truncated": truncated
    }))
}
