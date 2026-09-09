//! Connection visibility is separate from local readiness and upgrade health.
//! Only the supervised runtime publishes; readers match both PID and nonce.

pub fn publish(state: &str) {
    let Some(ready) = std::env::var_os("HEART_PORTAL_READY_FILE") else {
        return;
    };
    let Ok(nonce) = std::env::var("HEART_PORTAL_READY_NONCE") else {
        return;
    };
    let path = std::path::PathBuf::from(ready).with_file_name(".portal-connection-status.json");
    // No connection URL, token, or server-provided error text belongs here.
    let status = serde_json::json!({
        "pid": std::process::id(), "nonce": nonce, "state": state,
    });
    // Readers tolerate a partial/missing sample and retain their last state.
    // Telemetry failure must not stop a working connection or trigger rollback.
    if let Err(error) = std::fs::write(path, status.to_string()) {
        tracing::warn!("Could not publish Being connection status: {error}");
    }
}
