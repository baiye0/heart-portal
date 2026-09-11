use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Child;
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, error, warn};

use super::protocol::{JsonRpcRequest, JsonRpcResponse, McpToolInfo};
use super::{limits, ownership::ProcessOwner};

/// Default timeout for MCP handshake and metadata requests (initialize, tools/list).
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for tool calls — tools may run for minutes (code review, web fetch, etc.).
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(600);

/// A local admission failure must not terminate other calls on a healthy server.
#[derive(Debug)]
pub(crate) struct RequestRejected(&'static str);
impl std::fmt::Display for RequestRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for RequestRejected {}

#[derive(Debug)]
pub(crate) struct RemoteError {
    code: i32,
    message: String,
}
impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MCP server returned error: {} (code: {})",
            self.message, self.code
        )
    }
}
impl std::error::Error for RemoteError {}

#[derive(Debug)]
pub(crate) struct RequestTimeout(pub u64);
impl std::fmt::Display for RequestTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP request timed out after {} seconds; outcome is unknown, do not blindly retry writes", self.0)
    }
}
impl std::error::Error for RequestTimeout {}

// One host-wide budget for kit and custom MCP processes, including retired ones.
static PROCESS_CAPACITY: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

/// Configuration for a stdio MCP server process.
#[derive(Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<PathBuf>,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("name", &self.name)
            .field("command", &"<redacted>")
            .field("env", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// A stdio JSON-RPC connection to a single MCP server.
pub struct McpConnection {
    owner: Option<Arc<ProcessOwner>>,
    capacity: Vec<tokio::sync::OwnedSemaphorePermit>,
    child: Option<Child>,
    reader_task: Option<tokio::task::JoinHandle<()>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    cancel_task: Option<tokio::task::JoinHandle<()>>,
    cancellations: Option<tokio::sync::mpsc::Sender<u64>>,
    writer: Arc<Mutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
    responses: Arc<SyncMutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: AtomicU64,
    config: McpServerConfig,
    alive: Arc<AtomicBool>,
}

// Remove cancelled calls synchronously so reconnects cannot leak pending slots.
struct PendingRequest<'a> {
    connection: &'a McpConnection,
    id: u64,
    writing: bool,
    cancellable: bool,
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        let pending = self
            .connection
            .responses
            .lock()
            .unwrap()
            .remove(&self.id)
            .is_some();
        if self.writing {
            self.connection.abort();
        } else if pending && self.cancellable {
            if let Some(sender) = &self.connection.cancellations {
                if sender.try_send(self.id).is_err() {
                    self.connection.abort();
                }
            }
        }
    }
}

impl McpConnection {
    /// Spawn and initialize a stdio MCP server.
    pub async fn spawn(config: McpServerConfig) -> Result<Self> {
        if config.command.is_empty() {
            anyhow::bail!("Empty command for MCP server '{}'", config.name);
        }

        let permit = PROCESS_CAPACITY
            .get_or_init(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    super::limits::PROCESS_GENERATIONS,
                ))
            })
            .clone()
            .try_acquire_owned()
            .context("MCP process limit reached; retry after existing calls finish")?;
        let mut command = tokio::process::Command::new(&config.command[0]);
        // Do not hand community processes all host/service credentials.
        command.env_clear();
        for name in [
            "PATH",
            "PATHEXT",
            "SYSTEMROOT",
            "WINDIR",
            "COMSPEC",
            "TEMP",
            "TMP",
            "LOGNAME",
            "SHELL",
            "TERM",
            "TMPDIR",
            "TZ",
            "__CF_USER_TEXT_ENCODING",
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "PROGRAMDATA",
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "USER",
            "USERNAME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_RUNTIME_DIR",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "no_proxy",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
            .args(&config.command[1..])
            .envs(&config.env)
            .env("HEART_PORTAL_EXTERNAL_TOOL", "1")
            .env_remove("HEART_PORTAL_SUPERVISED")
            .env_remove("PORTAL_CONNECT_LINK")
            .env_remove("PORTAL_MCP_TOKEN")
            .env_remove("HEART_PORTAL_READY_FILE")
            .env_remove("HEART_PORTAL_READY_NONCE")
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }

        let (mut child, owner) = ProcessOwner::spawn(&mut command)
            .with_context(|| format!("Failed to spawn MCP server '{}'", config.name))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            anyhow::anyhow!("Failed to get stdin for MCP server '{}'", config.name)
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            anyhow::anyhow!("Failed to get stdout for MCP server '{}'", config.name)
        })?;

        let stderr = child.stderr.take();

        let responses = Arc::new(SyncMutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let mut connection = Self {
            owner: Some(Arc::new(owner)),
            capacity: vec![permit],
            child: Some(child),
            reader_task: None,
            stderr_task: None,
            cancel_task: None,
            cancellations: None,
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(stdin)))),
            responses: responses.clone(),
            next_id: AtomicU64::new(1),
            config,
            alive: alive.clone(),
        };

        let (sender, task) = super::cancellation::start(
            connection.writer.clone(),
            connection.owner.clone(),
            alive.clone(),
        );
        connection.cancellations = Some(sender);
        connection.cancel_task = Some(task);
        let server_name = connection.config.name.clone();
        let reader_owner = connection.owner.clone();
        connection.reader_task = Some(tokio::spawn(async move {
            if let Err(e) =
                Self::reader_task(BufReader::new(stdout), responses, alive, &server_name).await
            {
                error!("MCP server '{}' reader failed: {}", server_name, e);
            }
            if let Some(owner) = reader_owner {
                owner.terminate();
            }
        }));

        if let Some(stderr) = stderr {
            let server_name = connection.config.name.clone();
            let stderr_owner = connection.owner.clone();
            connection.stderr_task = Some(tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = Vec::new();
                let mut budget = limits::OutputBudget::new(1024 * 1024);
                let mut reported = false;
                loop {
                    match limits::read_line(&mut reader, &mut line, 64 * 1024).await {
                        Ok(0) => break,
                        Ok(count) => {
                            if budget.consume(count).is_err() {
                                warn!("MCP server '{}' exceeded stderr output limit", server_name);
                                if let Some(owner) = &stderr_owner {
                                    owner.terminate();
                                }
                                break;
                            }
                            if !reported {
                                warn!("MCP server '{}' wrote stderr; content omitted to protect credentials", server_name);
                                reported = true;
                            }
                        }
                        Err(err) => {
                            warn!("MCP server '{}' stderr read failed: {}", server_name, err);
                            if let Some(owner) = &stderr_owner {
                                owner.terminate();
                            }
                            break;
                        }
                    }
                }
            }));
        }

        if let Err(e) = connection.initialize().await {
            let mut connection = connection;
            let _ = connection.shutdown().await;
            return Err(e);
        }

        debug!(
            "MCP server '{}' spawned and initialized",
            connection.config.name
        );
        Ok(connection)
    }

    async fn reader_task<R>(
        reader: BufReader<R>,
        responses: Arc<SyncMutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
        alive: Arc<AtomicBool>,
        server_name: &str,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let result = Self::read_responses(reader, responses.clone(), server_name).await;
        // Mark dead before waking callers, on both EOF and read errors. Otherwise
        // a broken pipe/invalid UTF-8 can leave tool calls waiting for ten minutes.
        alive.store(false, Ordering::SeqCst);
        Self::drop_pending(responses, server_name).await;
        result
    }

    async fn read_responses<R>(
        mut reader: BufReader<R>,
        responses: Arc<SyncMutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
        server_name: &str,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut line = Vec::new();
        let mut budget = limits::OutputBudget::new(16 * 1024 * 1024);

        loop {
            let bytes_read = limits::read_line(&mut reader, &mut line, limits::MESSAGE_BYTES)
                .await
                .with_context(|| format!("Reading from MCP server '{}'", server_name))?;

            if bytes_read == 0 {
                debug!("MCP server '{}' connection closed", server_name);
                break;
            }

            budget.consume(bytes_read)?;
            let trimmed = std::str::from_utf8(&line)?.trim();
            if trimmed.is_empty() {
                continue;
            }

            debug!(
                "MCP server '{}' message ({} bytes)",
                server_name,
                trimmed.len()
            );

            let response: JsonRpcResponse = match serde_json::from_str(trimmed) {
                Ok(resp) => resp,
                Err(e) => {
                    warn!(
                        "MCP server '{}' sent invalid JSON at line {}, column {}",
                        server_name,
                        e.line(),
                        e.column()
                    );
                    continue;
                }
            };

            if let Some(id) = response.id {
                let mut pending = responses.lock().unwrap();
                if let Some(sender) = pending.remove(&id) {
                    if sender.send(response).is_err() {
                        warn!(
                            "MCP server '{}' response receiver dropped for id {}",
                            server_name, id
                        );
                    }
                } else {
                    warn!(
                        "MCP server '{}' sent response for unknown id {}",
                        server_name, id
                    );
                }
            } else {
                debug!("MCP server '{}' sent notification", server_name);
            }
        }

        Ok(())
    }

    async fn drop_pending(
        responses: Arc<SyncMutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
        server_name: &str,
    ) {
        let mut pending = responses.lock().unwrap();
        if !pending.is_empty() {
            warn!(
                "MCP server '{}' reader closed: dropping {} pending response(s)",
                server_name,
                pending.len()
            );
        }
        pending.clear();
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let request = JsonRpcRequest::new(method, params, &self.next_id);
        let request_id = request
            .id
            .ok_or_else(|| anyhow::anyhow!("MCP request missing id"))?;
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.responses.lock().unwrap();
            // Check under the same lock used by reader cleanup: either this
            // request is rejected, or cleanup will see and cancel it.
            if !self.is_alive() {
                anyhow::bail!(
                    "MCP server '{}': the kit process exited or closed stdout",
                    self.config.name
                );
            }
            if pending.len() >= limits::PENDING_REQUESTS {
                return Err(
                    RequestRejected("MCP server is busy; too many pending requests").into(),
                );
            }
            pending.insert(request_id, tx);
        }

        let mut pending_guard = PendingRequest {
            connection: self,
            id: request_id,
            writing: false,
            cancellable: method != "initialize",
        };
        let request_json = serde_json::to_string(&request).with_context(|| {
            format!("Serializing request for MCP server '{}'", self.config.name)
        })?;
        if request_json.len() > limits::MESSAGE_BYTES {
            self.responses.lock().unwrap().remove(&request_id);
            return Err(RequestRejected("MCP request exceeds the size limit").into());
        }

        debug!("MCP server '{}' request: {}", self.config.name, method);

        let write = tokio::time::timeout(Duration::from_secs(5), async {
            let mut writer = self.writer.lock().await;
            pending_guard.writing = true;
            let write_result = async {
                writer
                    .write_all(request_json.as_bytes())
                    .await
                    .with_context(|| format!("Writing to MCP server '{}'", self.config.name))?;
                writer.write_all(b"\n").await.with_context(|| {
                    format!("Writing newline to MCP server '{}'", self.config.name)
                })?;
                writer
                    .flush()
                    .await
                    .with_context(|| format!("Flushing MCP server '{}'", self.config.name))?;
                anyhow::Ok(())
            }
            .await;

            if let Err(e) = write_result {
                return Err(e);
            }
            Ok(())
        })
        .await;
        if !matches!(write, Ok(Ok(()))) {
            self.responses.lock().unwrap().remove(&request_id);
            // A cancelled partial JSON line cannot safely be resumed/retried.
            self.abort();
            anyhow::bail!("MCP request write failed or timed out");
        }

        pending_guard.writing = false;
        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(result) => result.with_context(|| {
                let state = if self.is_alive() {
                    "response receiver was dropped"
                } else {
                    "the kit process exited or closed stdout"
                };
                format!(
                    "Response channel closed for MCP server '{}': {}",
                    self.config.name, state
                )
            })?,
            Err(_) => {
                // The guard releases the slot and sends a bounded cancellation.
                return Err(RequestTimeout(timeout.as_secs()).into());
            }
        };

        if let Some(error) = response.error {
            return Err(RemoteError {
                code: error.code,
                message: error.message,
            }
            .into());
        }

        response.result.ok_or_else(|| {
            anyhow::anyhow!(
                "MCP server '{}' response missing result field",
                self.config.name
            )
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, DEFAULT_RPC_TIMEOUT)
            .await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let request = JsonRpcRequest::notification(method, params);
        let request_json = serde_json::to_string(&request).with_context(|| {
            format!(
                "Serializing notification for MCP server '{}'",
                self.config.name
            )
        })?;

        debug!("MCP server '{}' notification: {}", self.config.name, method);

        let mut writer = self.writer.lock().await;
        writer
            .write_all(request_json.as_bytes())
            .await
            .with_context(|| {
                format!("Writing notification to MCP server '{}'", self.config.name)
            })?;
        writer
            .write_all(b"\n")
            .await
            .with_context(|| format!("Writing newline to MCP server '{}'", self.config.name))?;
        writer.flush().await.with_context(|| {
            format!(
                "Flushing MCP server '{}' after notification",
                self.config.name
            )
        })?;

        Ok(())
    }

    async fn initialize(&self) -> Result<()> {
        debug!("Initializing MCP server '{}'", self.config.name);

        self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "clientInfo": {
                    "name": "heart-cortex",
                    "version": "1.0.0"
                }
            }),
        )
        .await?;

        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;

        debug!("MCP server '{}' initialization complete", self.config.name);
        Ok(())
    }

    /// Get the list of tools from this server.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let result = self.request("tools/list", serde_json::json!({})).await?;

        let tools = result["tools"].as_array().ok_or_else(|| {
            anyhow::anyhow!(
                "MCP server '{}' tools/list response missing 'tools' array",
                self.config.name
            )
        })?;

        anyhow::ensure!(tools.len() <= 128, "MCP server declared too many tools");
        let mut parsed_tools = Vec::new();
        for tool in tools {
            let tool_info: McpToolInfo =
                serde_json::from_value(tool.clone()).with_context(|| {
                    format!("Parsing tool info from MCP server '{}'", self.config.name)
                })?;
            anyhow::ensure!(
                !tool_info.name.is_empty()
                    && tool_info.name.len() <= 128
                    && tool_info
                        .name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')),
                "MCP server declared an invalid tool name"
            );
            parsed_tools.push(tool_info);
        }

        debug!(
            "MCP server '{}' provides {} tools",
            self.config.name,
            parsed_tools.len()
        );
        Ok(parsed_tools)
    }

    /// Call a tool on this server.
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<Value> {
        debug!(
            "Calling tool '{}' on MCP server '{}'",
            tool_name, self.config.name
        );

        let result = self
            .request_with_timeout(
                "tools/call",
                serde_json::json!({
                    "name": tool_name,
                    "arguments": arguments
                }),
                TOOL_CALL_TIMEOUT,
            )
            .await?;

        // Tool bodies can contain credentials and private service data.
        debug!(
            "Tool '{}' on MCP server '{}' completed",
            tool_name, self.config.name
        );
        Ok(result)
    }

    /// Check whether the connection reader task is still alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn process_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub(crate) fn set_capacity(&mut self, permits: Vec<tokio::sync::OwnedSemaphorePermit>) {
        self.capacity.extend(permits);
    }

    /// Synchronous ownership cleanup remains available when async tasks stall.
    pub(crate) fn abort(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(owner) = &self.owner {
            owner.terminate();
        }
    }

    /// Shutdown the child process.
    pub async fn shutdown(&mut self) -> Result<()> {
        debug!("Shutting down MCP server '{}'", self.config.name);
        if let Some(task) = self.cancel_task.take() {
            task.abort();
        }
        self.cancellations = None;
        if self.is_alive() {
            let closed = if let Ok(mut writer) = self.writer.try_lock() {
                *writer = BufWriter::new(Box::new(tokio::io::sink()));
                true
            } else {
                false
            };
            if closed {
                #[cfg(unix)]
                if let Some(owner) = &self.owner {
                    // Keep the group leader unreaped until abort signals its
                    // group. Reaping it here would allow its PGID to be reused.
                    let _ = tokio::time::timeout(Duration::from_millis(250), owner.wait_unreaped()).await;
                }
                #[cfg(not(unix))]
                if let Some(child) = &mut self.child {
                    let _ = tokio::time::timeout(Duration::from_millis(250), child.wait()).await;
                }
            }
        }
        // Force cleanup after the grace period, before waiting for pipes/locks.
        self.abort();

        // Publish the shutdown while holding the response-map lock used by new
        // requests, so none can slip in and wait on a connection being closed.
        {
            let mut pending = self.responses.lock().unwrap();
            self.alive.store(false, Ordering::SeqCst);
            if !pending.is_empty() {
                debug!(
                    "MCP server '{}' shutdown: cancelling {} pending response(s)",
                    self.config.name,
                    pending.len()
                );
            }
            pending.clear();
        }

        // Drop the child's stdin pipe first. This lets line-oriented servers
        // observe EOF and, on Windows, avoids retaining a pipe handle after the
        // process tree has been terminated.
        {
            let mut writer = self.writer.lock().await;
            if let Err(e) = writer.shutdown().await {
                warn!(
                    "Failed to close stdin for MCP server '{}': {}",
                    self.config.name, e
                );
            }
            *writer = BufWriter::new(Box::new(tokio::io::sink()));
        }

        if let Some(mut child) = self.child.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                Ok(Ok(status)) => {
                    debug!(
                        "MCP server '{}' process exited with status: {}",
                        self.config.name, status
                    );
                }
                Ok(Err(e)) => {
                    warn!(
                        "Error waiting for MCP server '{}' process: {}",
                        self.config.name, e
                    );
                }
                Err(_) => {
                    warn!(
                        "Timeout waiting for MCP server '{}' process to exit",
                        self.config.name
                    );
                    if let Err(e) = child.kill().await {
                        warn!(
                            "Failed to kill MCP server '{}' process: {}",
                            self.config.name, e
                        );
                    } else if let Err(e) = child.wait().await {
                        warn!(
                            "Error reaping MCP server '{}' process: {}",
                            self.config.name, e
                        );
                    }
                }
            }
        }

        // Child processes may still be releasing inherited stdout/stderr handles
        // after their launcher exits. Await the readers so Windows releases the
        // kit's working directory before callers remove or replace it.
        let reader_task = self.reader_task.take();
        let stderr_task = self.stderr_task.take();
        tokio::join!(
            finish_io_task(reader_task, "stdout", &self.config.name),
            finish_io_task(stderr_task, "stderr", &self.config.name),
        );

        Ok(())
    }
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.abort();
        if let Some(task) = &self.cancel_task {
            task.abort();
        }
        if let Some(task) = &self.reader_task {
            task.abort();
        }
        if let Some(task) = &self.stderr_task {
            task.abort();
        }
    }
}

async fn finish_io_task(
    task: Option<tokio::task::JoinHandle<()>>,
    stream: &str,
    server_name: &str,
) {
    let Some(mut task) = task else {
        return;
    };
    match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(
            "MCP server '{}' {} reader task failed: {}",
            server_name, stream, e
        ),
        Err(_) => {
            warn!(
                "Timeout waiting for MCP server '{}' {} reader task",
                server_name, stream
            );
            task.abort();
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_call_releases_pending_slot_without_killing_healthy_connection() {
        use tokio::io::AsyncBufReadExt;
        let (writer, peer) = tokio::io::duplex(4096);
        let connection = Arc::new(McpConnection {
            owner: None,
            capacity: Vec::new(),
            child: None,
            reader_task: None,
            stderr_task: None,
            cancel_task: None,
            cancellations: None,
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(writer)))),
            responses: Arc::new(SyncMutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            config: McpServerConfig {
                name: "cancelled".into(),
                command: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            alive: Arc::new(AtomicBool::new(true)),
        });
        let mut peer = BufReader::new(peer);
        // More cancellations than the pending limit must not exhaust admission.
        for _ in 0..20 {
            let calling = connection.clone();
            let task =
                tokio::spawn(
                    async move { calling.call_tool("example", serde_json::json!({})).await },
                );
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(1), peer.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(connection.responses.lock().unwrap().is_empty());
            assert!(connection.is_alive());
        }
        let mut line = String::new();
        let (result, _) = tokio::join!(
            connection.request_with_timeout(
                "tools/call",
                serde_json::json!({}),
                Duration::from_millis(10)
            ),
            peer.read_line(&mut line),
        );
        assert!(result.unwrap_err().is::<RequestTimeout>());
        assert!(connection.responses.lock().unwrap().is_empty());
        assert!(connection.is_alive());
    }

    #[tokio::test]
    async fn request_after_reader_closed_fails_without_waiting_or_writing() {
        let (writer, mut peer) = tokio::io::duplex(4096);
        let connection = McpConnection {
            owner: None,
            capacity: Vec::new(),
            child: None,
            reader_task: None,
            stderr_task: None,
            cancel_task: None,
            cancellations: None,
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(writer)))),
            responses: Arc::new(SyncMutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            config: McpServerConfig {
                name: "closed-reader".into(),
                command: vec![],
                env: HashMap::new(),
                cwd: None,
            },
            alive: Arc::new(AtomicBool::new(true)),
        };
        McpConnection::reader_task(
            BufReader::new(&b""[..]),
            connection.responses.clone(),
            connection.alive.clone(),
            "closed-reader",
        )
        .await
        .unwrap();
        // The kit may close stdout while keeping stdin open. A successful write
        // must not make a new request wait for the normal ten-minute timeout.
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            connection.call_tool("example", serde_json::json!({})),
        )
        .await
        .expect("closed reader must fail immediately")
        .unwrap_err();
        assert!(error.to_string().contains("closed stdout"), "{error}");
        assert!(connection.responses.lock().unwrap().is_empty());
        drop(connection);
        let mut output = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut peer, &mut output)
            .await
            .unwrap();
        assert!(output.is_empty(), "must not write to a dead connection");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cmd_kit_handles_spaces_and_literal_metacharacters() {
        let dir = std::env::temp_dir().join(format!("portal kit ({})", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let _fixture_dir = crate::kits::tests::TestKits(dir.clone());
        let script = dir.join("kit shim.cmd");
        std::fs::write(
            &script,
            concat!(
                "@echo off\r\n",
                "set \"PORTAL_TEST_KIT_TOKEN=%~1\"\r\n",
                "\"%PORTAL_TEST_KIT_BINARY%\" --exact kits::tests::mcp_fixture --nocapture --quiet\r\n",
            ),
        )
        .unwrap();
        // Reuse the native MCP fixture so this argument-quoting regression does
        // not depend on PowerShell's cold startup or module discovery on CI.
        let mut connection = McpConnection::spawn(McpServerConfig {
            name: "cmd-test".into(),
            command: vec![
                script.to_string_lossy().into_owned(),
                "space & value".into(),
            ],
            env: HashMap::from([
                ("PORTAL_TEST_KIT_FIXTURE".into(), "1".into()),
                (
                    "PORTAL_TEST_KIT_BINARY".into(),
                    std::env::current_exe().unwrap().to_string_lossy().into_owned(),
                ),
            ]),
            cwd: Some(dir.clone()),
        })
        .await
        .unwrap();
        let result = connection
            .request_with_timeout(
                "tools/call",
                serde_json::json!({"name": "ping", "arguments": {}}),
                Duration::from_secs(5),
            )
            .await;
        connection.shutdown().await.unwrap();
        assert_eq!(result.unwrap()["token"], "space & value");
    }

    #[tokio::test]
    async fn closed_reader_drops_pending_on_eof_and_read_error() {
        for input in [&b""[..], &b"\xff\n"[..]] {
            let (sender, receiver) = oneshot::channel();
            let responses = Arc::new(SyncMutex::new(HashMap::from([(1, sender)])));
            let alive = Arc::new(AtomicBool::new(true));
            let result = McpConnection::reader_task(
                BufReader::new(input),
                responses.clone(),
                alive.clone(),
                "test",
            )
            .await;
            assert_eq!(result.is_err(), !input.is_empty());
            assert!(!alive.load(Ordering::SeqCst));
            assert!(responses.lock().unwrap().is_empty());
            assert!(receiver.await.is_err());
        }
    }
}
