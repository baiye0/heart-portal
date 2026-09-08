//! Background process manager for portal_exec (background) + portal_process tools.

use crate::config::PortalConfig;
use crate::exec_policy::{configure_shell_command, validate_exec_allowlist, ExecShell};
use crate::tools::text::{OutputDecoder, OutputEncoding};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time;
use tracing::{debug, info, warn};

const DEFAULT_MAX_SESSIONS: usize = 10;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const KILL_GRACE: Duration = Duration::from_secs(5);
const EXIT_RETENTION: Duration = Duration::from_secs(5 * 60);
/// Long-poll cap: prevents MCP clients from holding connections indefinitely.
pub const MAX_POLL_TIMEOUT_MS: u64 = 300_000;
/// Single stdin write cap (interactive prompts).
pub const MAX_STDIN_WRITE_BYTES: usize = 256 * 1024;
/// Session ids are `sess_` + UUID; reject oversized / odd keys.
pub const MAX_SESSION_ID_BYTES: usize = 128;

/// Tail of the output ring buffer attached to a callback payload.
/// 200KB leaves ~56KB of headroom for the other payload fields (cap 256KB).
const CALLBACK_OUTPUT_MAX_BYTES: usize = 200 * 1024;
/// Smaller tail used when the JSON-escaped payload still exceeds the cap.
const CALLBACK_OUTPUT_FALLBACK_BYTES: usize = 128 * 1024;
/// Hard cap on the serialized callback payload.
const CALLBACK_PAYLOAD_MAX_BYTES: usize = 256 * 1024;
/// The command is being-supplied and otherwise unbounded; keep it from eating
/// the payload budget that the output tail is sized against.
const CALLBACK_COMMAND_MAX_BYTES: usize = 4096;
/// 1 initial attempt + 2 retries.
const CALLBACK_ATTEMPTS: usize = 3;
/// Backoff before retry N (index 0 = before the 2nd attempt).
const CALLBACK_BACKOFF: [Duration; CALLBACK_ATTEMPTS - 1] =
    [Duration::from_secs(2), Duration::from_secs(4)];
/// Bound the wait when descendants retain the exited process's pipe handles.
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const CALLBACK_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where finished background sessions are reported (Heart's `POST /api/callback`).
/// Present only in `--connect` mode; `None` means standalone (no callback).
#[derive(Clone)]
pub struct CallbackConfig {
    pub url: String,
    pub token: String,
    pub portal_name: String,
    pub client: reqwest::Client,
}

pub struct ProcessManager {
    sessions: Arc<AsyncMutex<HashMap<String, ManagedProcess>>>,
    max_sessions: usize,
    max_output_bytes: usize,
    callback_config: Arc<Mutex<Option<CallbackConfig>>>,
}

pub struct ManagedProcess {
    pub session_id: String,
    pub pid: u32,
    pub command: String,
    pub started_at: tokio::time::Instant,
    pub stdin: Option<ChildStdin>,
    pub output: Arc<AsyncMutex<OutputBuffer>>,
    pub status: Arc<AsyncMutex<ProcessStatus>>,
    pub output_encoding: OutputEncoding,
    /// Set before we signal the process; suppresses the exit callback so
    /// `portal_process kill` and Portal shutdown never wake the being.
    pub(crate) killed: Arc<AtomicBool>,
    pub(crate) notify: Arc<Notify>,
    pub(crate) exited_at: Option<tokio::time::Instant>,
}

pub struct OutputBuffer {
    /// Normalized UTF-8, always beginning and ending on character boundaries.
    pub data: Vec<u8>,
    pub max_bytes: usize,
    total_written: u64,
    /// Last time stdout/stderr delivered bytes (spawn time until first byte).
    last_output_at: time::Instant,
}

impl OutputBuffer {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            data: Vec::new(),
            max_bytes,
            total_written: 0,
            last_output_at: time::Instant::now(),
        }
    }

    pub fn total_written(&self) -> u64 {
        self.total_written
    }

    /// Seconds since last captured output (or since buffer creation if none yet).
    pub fn idle_s(&self) -> u64 {
        self.last_output_at.elapsed().as_secs()
    }

    pub fn append(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.last_output_at = time::Instant::now();
        self.total_written += chunk.len() as u64;
        self.data.extend_from_slice(chunk.as_bytes());
        if self.data.len() > self.max_bytes {
            let mut drop = self.data.len() - self.max_bytes;
            while !self.is_char_boundary(drop) {
                drop += 1;
            }
            self.data.drain(..drop);
        }
    }

    fn is_char_boundary(&self, index: usize) -> bool {
        index == self.data.len() || self.data[index] & 0xc0 != 0x80
    }

    /// Returns bytes from logical `offset` to current end, whether data was dropped before `offset`, and `total_written`.
    pub fn bytes_since(&self, offset: u64) -> Result<(Vec<u8>, bool, u64)> {
        let start_offset = self.total_written.saturating_sub(self.data.len() as u64);
        let truncated = offset < start_offset;
        let from = offset.max(start_offset);
        if from >= self.total_written || self.data.is_empty() {
            return Ok((vec![], truncated, self.total_written));
        }
        let start_idx = (from - start_offset) as usize;
        anyhow::ensure!(
            self.is_char_boundary(start_idx),
            "offset must be a UTF-8 character boundary; use next_offset from the previous response"
        );
        Ok((
            self.data[start_idx..].to_vec(),
            truncated,
            self.total_written,
        ))
    }

    pub fn bytes_range(&self, offset: u64, limit: usize) -> Result<(Vec<u8>, u64)> {
        anyhow::ensure!(limit > 0, "limit must be positive");
        let start_offset = self.total_written.saturating_sub(self.data.len() as u64);
        let from = offset.max(start_offset);
        if from >= self.total_written || self.data.is_empty() {
            return Ok((vec![], self.total_written));
        }
        let start_idx = (from - start_offset) as usize;
        anyhow::ensure!(
            self.is_char_boundary(start_idx),
            "offset must be a UTF-8 character boundary; use next_offset from the previous response"
        );
        let mut end = start_idx.saturating_add(limit).min(self.data.len());
        while !self.is_char_boundary(end) {
            end -= 1;
        }
        anyhow::ensure!(
            end > start_idx,
            "limit is too small for the next UTF-8 character; use at least 4 bytes"
        );
        let next_offset = start_offset + end as u64;
        Ok((self.data[start_idx..end].to_vec(), next_offset))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Exited(i32),
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub session_id: String,
    pub pid: u32,
    pub command: String,
    pub status: ProcessStatus,
    pub uptime_s: u64,
    /// Seconds since last stdout/stderr chunk (sensory: silence vs progress).
    pub idle_s: u64,
    /// Total normalized UTF-8 bytes (may exceed ring size; monotonic).
    pub total_output_bytes: u64,
    pub output_encoding: OutputEncoding,
}

#[derive(Clone, Debug)]
pub struct PollResult {
    pub output: Vec<u8>,
    pub next_offset: u64,
    pub truncated: bool,
    pub status: ProcessStatus,
    pub idle_s: u64,
    pub total_output_bytes: u64,
    pub output_encoding: OutputEncoding,
}

#[derive(Clone, Debug)]
pub struct LogResult {
    pub output: Vec<u8>,
    pub next_offset: u64,
    pub truncated: bool,
    pub status: ProcessStatus,
    pub idle_s: u64,
    pub total_output_bytes: u64,
    pub output_encoding: OutputEncoding,
}

pub fn validate_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() || session_id.len() > MAX_SESSION_ID_BYTES {
        anyhow::bail!("Invalid session_id");
    }
    if !session_id.starts_with("sess_") {
        anyhow::bail!("Invalid session_id");
    }
    Ok(())
}

async fn read_into_buffer<R: tokio::io::AsyncRead + Unpin>(
    mut stream: R,
    output: Arc<AsyncMutex<OutputBuffer>>,
    notify: Arc<Notify>,
    encoding: OutputEncoding,
) {
    let mut decoder = OutputDecoder::new(encoding);
    let mut buf = [0u8; 8192];
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        let text = decoder.push(&buf[..n], false);
        let mut o = output.lock().await;
        o.last_output_at = time::Instant::now();
        o.append(&text);
        drop(o);
        notify.notify_waiters();
    }
    // EOF (or a read error) is final, even for an unterminated line or an
    // incomplete source character. Poll/log/callback all see this same text.
    let remaining = decoder.push(&[], true);
    output.lock().await.append(&remaining);
    notify.notify_waiters();
}

/// A finished background session, as reported to Heart.
#[derive(Clone, Debug)]
struct CallbackTask {
    session_id: String,
    command: String,
    workdir: String,
    exit_code: i32,
    elapsed_secs: u64,
    output_encoding: OutputEncoding,
}

/// Last `max` bytes of `data` (tail — the interesting end of a build/test log).
fn tail(data: &[u8], max: usize) -> &[u8] {
    let mut start = data.len().saturating_sub(max);
    while start < data.len() && data[start] & 0xc0 == 0x80 {
        start += 1;
    }
    &data[start..]
}

/// Head of `s` capped at `max` bytes, never splitting a UTF-8 char.
fn clamp_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

fn payload_with_tail(
    task: &CallbackTask,
    portal_name: &str,
    data: &[u8],
    total_output_bytes: u64,
    max_output: usize,
) -> serde_json::Value {
    let slice = tail(data, max_output);
    let truncated = (slice.len() as u64) < total_output_bytes;
    let command = clamp_str(&task.command, CALLBACK_COMMAND_MAX_BYTES);
    serde_json::json!({
        "source": "portal",
        "task_id": task.session_id,
        "summary": format!(
            "portal_exec completed: '{}' (exit {})",
            command, task.exit_code
        ),
        "result": {
            "session_id": task.session_id,
            "exit_code": task.exit_code,
            "output": String::from_utf8_lossy(slice),
            "output_encoding": task.output_encoding.as_str(),
            "command": command,
            "workdir": task.workdir,
            "elapsed_secs": task.elapsed_secs,
            "portal_name": portal_name,
            "truncated": truncated,
            "total_output_bytes": total_output_bytes,
        }
    })
}

/// Build the `POST /api/callback` body. Output is the *tail* of the ring buffer;
/// if JSON escaping still blows past the payload cap, fall back to a shorter tail.
fn build_callback_payload(
    task: &CallbackTask,
    portal_name: &str,
    data: &[u8],
    total_output_bytes: u64,
) -> serde_json::Value {
    let payload = payload_with_tail(
        task,
        portal_name,
        data,
        total_output_bytes,
        CALLBACK_OUTPUT_MAX_BYTES,
    );
    let size = serde_json::to_vec(&payload)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if size <= CALLBACK_PAYLOAD_MAX_BYTES {
        return payload;
    }
    payload_with_tail(
        task,
        portal_name,
        data,
        total_output_bytes,
        CALLBACK_OUTPUT_FALLBACK_BYTES,
    )
}

/// Retry 5xx (Heart restarting / proxy hiccup); never retry 4xx (401, 413, …).
fn should_retry_status(status: u16) -> bool {
    status >= 500
}

/// Retry transport failures that a later attempt may survive.
fn should_retry_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout()
}

/// Strip the query string so a token can never reach the logs.
fn redact_url(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?<redacted>"),
        None => url.to_string(),
    }
}

/// POST the payload with `Authorization: Bearer`, retrying per PRD §3.
/// Never returns an error: a lost callback is a WARN, not a Portal failure
/// (the being can still `portal_process poll`).
async fn deliver_callback(cfg: CallbackConfig, session_id: String, payload: serde_json::Value) {
    let mut last_err = String::from("no attempt made");
    for attempt in 0..CALLBACK_ATTEMPTS {
        if attempt > 0 {
            time::sleep(CALLBACK_BACKOFF[attempt - 1]).await;
        }
        // Heart checks `?token=` query param, not Authorization header.
        let url_with_token = format!("{}?token={}", cfg.url, cfg.token);
        match cfg
            .client
            .post(&url_with_token)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    debug!("callback delivered for session {session_id} ({status})");
                    return;
                }
                last_err = format!("HTTP {status}");
                if !should_retry_status(status.as_u16()) {
                    break;
                }
            }
            Err(e) => {
                let retry = should_retry_error(&e);
                last_err = e.without_url().to_string();
                if !retry {
                    break;
                }
            }
        }
    }
    warn!(
        "callback delivery failed for session {} to {}: {}",
        session_id,
        redact_url(&cfg.url),
        last_err
    );
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(AsyncMutex::new(HashMap::new())),
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            callback_config: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable async callbacks: finished background sessions POST their result to
    /// `url` with `Authorization: Bearer <token>`. Called from `--connect` mode
    /// before the relay handshake. Without it, sessions exit silently.
    pub fn set_callback_config(&self, url: String, token: String, portal_name: String) {
        let client = match reqwest::Client::builder()
            .timeout(CALLBACK_HTTP_TIMEOUT)
            .connect_timeout(CALLBACK_CONNECT_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to build callback HTTP client: {e}; async callbacks disabled");
                return;
            }
        };
        info!("async callback enabled → {}", redact_url(&url));
        *self.callback_config.lock().unwrap() = Some(CallbackConfig {
            url,
            token,
            portal_name,
            client,
        });
    }

    #[cfg(test)]
    pub async fn spawn(
        &self,
        config: &PortalConfig,
        command: &str,
        workdir: &str,
        extra_env: &[(String, String)],
    ) -> Result<SessionInfo> {
        self.spawn_with_shell(
            config,
            command,
            workdir,
            extra_env,
            ExecShell::Default,
            OutputEncoding::Auto.for_shell(ExecShell::Default),
        )
        .await
    }

    pub(crate) async fn spawn_with_shell(
        &self,
        config: &PortalConfig,
        command: &str,
        workdir: &str,
        extra_env: &[(String, String)],
        shell: ExecShell,
        output_encoding: OutputEncoding,
    ) -> Result<SessionInfo> {
        validate_exec_allowlist(command, &config.security.exec_allowlist)?;

        let running = {
            let g = self.sessions.lock().await;
            let mut n = 0;
            for s in g.values() {
                if matches!(*s.status.lock().await, ProcessStatus::Running) {
                    n += 1;
                }
            }
            n
        };
        if running >= self.max_sessions {
            anyhow::bail!(
                "Maximum concurrent background sessions ({}) reached",
                self.max_sessions
            );
        }

        let session_id = format!("sess_{}", uuid::Uuid::new_v4());
        let output = Arc::new(AsyncMutex::new(OutputBuffer::new(self.max_output_bytes)));
        let status = Arc::new(AsyncMutex::new(ProcessStatus::Running));
        let notify = Arc::new(Notify::new());

        let mut cmd = Command::new(shell.program());
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        configure_shell_command(&mut cmd, command, config, workdir, shell);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn: {}", e))?;
        let pid = child.id().unwrap_or_else(|| {
            tracing::warn!("Failed to get child process ID");
            0
        });
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdout not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("stderr not piped"))?;

        let out_a = Arc::clone(&output);
        let n_a = Arc::clone(&notify);
        let stdout_reader = tokio::spawn(async move {
            read_into_buffer(stdout, out_a, n_a, output_encoding).await;
        });
        let out_b = Arc::clone(&output);
        let n_b = Arc::clone(&notify);
        let stderr_reader = tokio::spawn(async move {
            read_into_buffer(stderr, out_b, n_b, output_encoding).await;
        });

        let started_at = tokio::time::Instant::now();
        let killed = Arc::new(AtomicBool::new(false));

        // Register before the exit watcher runs, even for very short commands.
        let proc = ManagedProcess {
            session_id: session_id.clone(),
            pid,
            command: command.to_string(),
            started_at,
            stdin,
            output: Arc::clone(&output),
            status: Arc::clone(&status),
            output_encoding,
            killed: Arc::clone(&killed),
            notify: Arc::clone(&notify),
            exited_at: None,
        };
        self.sessions.lock().await.insert(session_id.clone(), proc);

        let st_b = Arc::clone(&status);
        let n_exit = Arc::clone(&notify);
        let sessions_wait = Arc::clone(&self.sessions);
        let sid_wait = session_id.clone();
        let cmd_wait = command.to_string();
        let workdir_wait = workdir.to_string();
        let output_wait = Arc::clone(&output);
        let callback_config = Arc::clone(&self.callback_config);
        let killed_wait = Arc::clone(&killed);
        let callback_output_encoding = output_encoding;
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(s) => s.code().unwrap_or_else(|| {
                    tracing::debug!("Process terminated by signal, no exit code available");
                    -1
                }),
                Err(_) => -1,
            };
            let now = tokio::time::Instant::now();
            {
                let mut g = sessions_wait.lock().await;
                if let Some(p) = g.get_mut(&sid_wait) {
                    p.exited_at = Some(now);
                }
            }
            // EOF must flush both decoders before an ordinary exit is exposed
            // to poll/log, including standalone sessions without callbacks.
            if time::timeout(OUTPUT_DRAIN_TIMEOUT, async {
                let _ = stdout_reader.await;
                let _ = stderr_reader.await;
            })
            .await
            .is_err()
            {
                warn!(
                    "session {sid_wait}: output drain timed out; descendants may still hold pipes"
                );
            }
            let mut st = st_b.lock().await;
            *st = ProcessStatus::Exited(code);
            drop(st);

            // Pollers first: they are waiting on this notify right now.
            n_exit.notify_waiters();

            // kill / shutdown are deliberate — never wake the being for them.
            if killed_wait.load(Ordering::SeqCst) {
                debug!("session {sid_wait} was killed; skipping callback");
                return;
            }
            let Some(cfg) = callback_config.lock().unwrap().clone() else {
                return;
            };

            let task = CallbackTask {
                session_id: sid_wait.clone(),
                command: cmd_wait,
                workdir: workdir_wait,
                exit_code: code,
                elapsed_secs: now.saturating_duration_since(started_at).as_secs(),
                output_encoding: callback_output_encoding,
            };
            // Detached: the exit watcher must never wait on the network.
            tokio::spawn(async move {
                let (data, total) = {
                    let buf = output_wait.lock().await;
                    (buf.data.clone(), buf.total_written())
                };
                let payload = build_callback_payload(&task, &cfg.portal_name, &data, total);
                deliver_callback(cfg, task.session_id, payload).await;
            });
        });

        debug!(
            "spawned background session {} pid {} ({})",
            session_id, pid, command
        );

        Ok(SessionInfo {
            session_id,
            pid,
            command: command.to_string(),
            status: ProcessStatus::Running,
            uptime_s: 0,
            idle_s: 0,
            total_output_bytes: 0,
            output_encoding,
        })
    }

    pub async fn poll(
        &self,
        session_id: &str,
        offset: u64,
        timeout_ms: u64,
    ) -> Result<PollResult> {
        validate_session_id(session_id)?;
        let timeout_ms = timeout_ms.min(MAX_POLL_TIMEOUT_MS);
        let deadline = if timeout_ms > 0 {
            Some(time::Instant::now() + Duration::from_millis(timeout_ms))
        } else {
            None
        };

        loop {
            let (bytes, truncated, next, st, notify, idle_s, total_out, output_encoding) = {
                let guard = self.sessions.lock().await;
                let s = guard
                    .get(session_id)
                    .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
                let st = s.status.lock().await.clone();
                let (bytes, truncated, next, idle_s, total_out) = {
                    let buf = s.output.lock().await;
                    let (bytes, truncated, next) = buf.bytes_since(offset)?;
                    let idle_s = buf.idle_s();
                    let total_out = buf.total_written();
                    (bytes, truncated, next, idle_s, total_out)
                };
                let n = Arc::clone(&s.notify);
                (
                    bytes,
                    truncated,
                    next,
                    st,
                    n,
                    idle_s,
                    total_out,
                    s.output_encoding,
                )
            };

            if !bytes.is_empty() || matches!(st, ProcessStatus::Exited(_)) {
                return Ok(PollResult {
                    output: bytes,
                    next_offset: next,
                    truncated,
                    status: st,
                    idle_s,
                    total_output_bytes: total_out,
                    output_encoding,
                });
            }

            let Some(dl) = deadline else {
                return Ok(PollResult {
                    output: vec![],
                    next_offset: next,
                    truncated,
                    status: st,
                    idle_s,
                    total_output_bytes: total_out,
                    output_encoding,
                });
            };

            if time::Instant::now() >= dl {
                return Ok(PollResult {
                    output: vec![],
                    next_offset: next,
                    truncated,
                    status: st,
                    idle_s,
                    total_output_bytes: total_out,
                    output_encoding,
                });
            }

            let sleep = time::sleep_until(dl);
            tokio::select! {
                _ = notify.notified() => {}
                _ = sleep => {}
            }
        }
    }

    pub async fn log(&self, session_id: &str, offset: u64, limit: u64) -> Result<LogResult> {
        validate_session_id(session_id)?;
        let limit = limit.min(self.max_output_bytes as u64) as usize;
        let guard = self.sessions.lock().await;
        let s = guard
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
        let st = s.status.lock().await.clone();
        let (output, next_offset, truncated, idle_s, total_out) = {
            let buf = s.output.lock().await;
            let idle_s = buf.idle_s();
            let total_out = buf.total_written();
            let (output, next_offset) = buf.bytes_range(offset, limit)?;
            let start_offset = buf.total_written().saturating_sub(buf.data.len() as u64);
            let truncated = offset < start_offset;
            (output, next_offset, truncated, idle_s, total_out)
        };
        Ok(LogResult {
            output,
            next_offset,
            truncated,
            status: st,
            idle_s,
            total_output_bytes: total_out,
            output_encoding: s.output_encoding,
        })
    }

    pub async fn write_stdin(&self, session_id: &str, data: &[u8]) -> Result<()> {
        validate_session_id(session_id)?;
        if data.len() > MAX_STDIN_WRITE_BYTES {
            anyhow::bail!(
                "stdin write exceeds max {} bytes",
                MAX_STDIN_WRITE_BYTES
            );
        }
        let mut guard = self.sessions.lock().await;
        let s = guard
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
        if s.exited_at.is_some() || matches!(*s.status.lock().await, ProcessStatus::Exited(_)) {
            anyhow::bail!("Session has exited");
        }
        let stdin = s
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin not available for this session"))?;
        stdin.write_all(data).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn kill(&self, session_id: &str) -> Result<()> {
        validate_session_id(session_id)?;
        let pid = {
            let guard = self.sessions.lock().await;
            let s = guard
                .get(session_id)
                .ok_or_else(|| anyhow::anyhow!("Unknown session: {}", session_id))?;
            // Set before any signal: the exit watcher may run the moment we signal.
            s.killed.store(true, Ordering::SeqCst);
            // Output may still be draining after child.wait() reaped the PID.
            // Never signal that PID again: it may already have been reused.
            if s.exited_at.is_some() || matches!(*s.status.lock().await, ProcessStatus::Exited(_)) {
                return Ok(());
            }
            anyhow::ensure!(s.pid != 0, "Cannot kill a session without a process ID");
            s.pid
        };

        #[cfg(unix)]
        {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            time::sleep(KILL_GRACE).await;
            let still_running = {
                let guard = self.sessions.lock().await;
                if let Some(s) = guard.get(session_id) {
                    s.exited_at.is_none()
                        && matches!(*s.status.lock().await, ProcessStatus::Running)
                } else {
                    false
                }
            };
            if still_running {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/T", "/PID", &pid.to_string()])
                .creation_flags(0x08000000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            time::sleep(KILL_GRACE).await;
            let still_running = {
                let guard = self.sessions.lock().await;
                if let Some(s) = guard.get(session_id) {
                    s.exited_at.is_none()
                        && matches!(*s.status.lock().await, ProcessStatus::Running)
                } else {
                    false
                }
            };
            if still_running {
                let _ = Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .creation_flags(0x08000000)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = pid;
            anyhow::bail!("kill is not supported on this platform");
        }

        Ok(())
    }

    pub async fn list(&self) -> Vec<SessionInfo> {
        let guard = self.sessions.lock().await;
        let mut out = Vec::new();
        for s in guard.values() {
            let st = s.status.lock().await.clone();
            let uptime_s = match &st {
                ProcessStatus::Running => s.started_at.elapsed().as_secs(),
                ProcessStatus::Exited(_) => s
                    .exited_at
                    .map(|ex| ex.saturating_duration_since(s.started_at).as_secs())
                    .unwrap_or(0),
            };
            let (idle_s, total_output_bytes) = {
                let buf = s.output.lock().await;
                (buf.idle_s(), buf.total_written())
            };
            out.push(SessionInfo {
                session_id: s.session_id.clone(),
                pid: s.pid,
                command: s.command.clone(),
                status: st,
                uptime_s,
                idle_s,
                total_output_bytes,
                output_encoding: s.output_encoding,
            });
        }
        out
    }

    pub async fn cleanup(&self) {
        let now = tokio::time::Instant::now();
        let keys: Vec<String> = {
            let guard = self.sessions.lock().await;
            guard.keys().cloned().collect()
        };
        for k in keys {
            let remove = {
                let guard = self.sessions.lock().await;
                let Some(p) = guard.get(&k) else {
                    continue;
                };
                let st = p.status.lock().await.clone();
                match st {
                    ProcessStatus::Running => false,
                    ProcessStatus::Exited(_) => {
                        if let Some(ex) = p.exited_at {
                            now.duration_since(ex) >= EXIT_RETENTION
                        } else {
                            false
                        }
                    }
                }
            };
            if remove {
                let mut guard = self.sessions.lock().await;
                guard.remove(&k);
            }
        }
    }

    pub async fn kill_all(&self) {
        let ids: Vec<String> = {
            let g = self.sessions.lock().await;
            // Mark everything up front: `kill` below is sequential (5s grace each),
            // and a session must not fire a callback while waiting its turn.
            for s in g.values() {
                s.killed.store(true, Ordering::SeqCst);
            }
            g.keys().cloned().collect()
        };
        for id in ids {
            let _ = self.kill(&id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_buffer_ring_and_offsets() {
        let mut b = OutputBuffer::new(10);
        b.append("0123456789");
        let (chunk, trunc, n) = b.bytes_since(0).unwrap();
        assert!(!trunc);
        assert_eq!(n, 10);
        assert_eq!(chunk, b"0123456789");

        b.append("ABCDE");
        assert_eq!(b.data.len(), 10);
        let (_, trunc, n2) = b.bytes_since(0).unwrap();
        assert!(trunc);
        assert_eq!(n2, 15);
        let (chunk2, _, _) = b.bytes_since(10).unwrap();
        assert_eq!(chunk2, b"ABCDE");

        let (range, next) = b.bytes_range(7, 3).unwrap();
        assert_eq!(range, b"789");
        assert_eq!(next, 10, "range cursor must not skip bytes after its limit");
    }

    #[test]
    fn output_ring_and_callback_tail_never_split_utf8() {
        let text = "a中🙂éz";
        for max in 0..=text.len() {
            let mut buffer = OutputBuffer::new(max);
            buffer.append("a中");
            buffer.append("🙂éz");
            let retained = std::str::from_utf8(&buffer.data).unwrap();
            assert!(text.ends_with(retained));
            assert!(buffer.data.len() <= max);
            assert_eq!(buffer.total_written(), text.len() as u64);
            let (bytes, truncated, next) = buffer.bytes_since(0).unwrap();
            assert_eq!(bytes, buffer.data);
            assert_eq!(truncated, max < text.len());
            assert_eq!(next, text.len() as u64);
            assert!(text.ends_with(std::str::from_utf8(tail(text.as_bytes(), max)).unwrap()));
        }
        let mut task = sample_task();
        task.output_encoding = OutputEncoding::Oem;
        let payload = payload_with_tail(&task, "p", text.as_bytes(), text.len() as u64, 8);
        assert_eq!(
            payload["result"]["output"], "🙂éz",
            "normalized output must not be decoded twice"
        );
    }

    #[test]
    fn log_pagination_makes_progress_or_reports_an_invalid_boundary() {
        let mut buffer = OutputBuffer::new(64);
        buffer.append("中🙂z");
        assert!(buffer.bytes_range(0, 0).is_err());
        assert!(buffer.bytes_range(0, 2).is_err());
        assert!(buffer.bytes_range(1, 4).is_err());
        assert!(buffer.bytes_since(1).is_err());
        let (first, next) = buffer.bytes_range(0, 4).unwrap();
        assert_eq!(first, "中".as_bytes());
        assert_eq!(next, 3);
        let (second, next) = buffer.bytes_range(next, 4).unwrap();
        assert_eq!(second, "🙂".as_bytes());
        assert_eq!(next, 7);
        let (last, next) = buffer.bytes_range(next, usize::MAX).unwrap();
        assert_eq!(last, b"z");
        assert_eq!(next, 8);
        assert_eq!(buffer.bytes_range(next, 4).unwrap(), (vec![], next));
    }

    #[tokio::test]
    async fn pipe_reader_flushes_eof_and_keeps_streams_separate() {
        let output = Arc::new(AsyncMutex::new(OutputBuffer::new(64)));
        let notify = Arc::new(Notify::new());
        let (mut writer, reader) = tokio::io::duplex(1);
        let task = tokio::spawn(read_into_buffer(
            reader,
            output.clone(),
            notify.clone(),
            OutputEncoding::Utf8,
        ));
        writer.write_all(b"\xe4\xb8").await.unwrap();
        // stderr must not be spliced into the middle of stdout's character.
        read_into_buffer(
            b"error".as_slice(),
            output.clone(),
            notify,
            OutputEncoding::Utf8,
        )
        .await;
        writer.write_all(b"\xad\xf0\x9f").await.unwrap();
        drop(writer);
        task.await.unwrap();
        assert_eq!(output.lock().await.data, "error中�".as_bytes());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_reader_flushes_unterminated_oem_line_at_eof() {
        if crate::tools::text::windows_oem_code_page() != 936 {
            return; // Fixed-CP936 decoder tests run on every Windows locale.
        }
        let output = Arc::new(AsyncMutex::new(OutputBuffer::new(64)));
        read_into_buffer(
            b"\xe4\xb8".as_slice(),
            output.clone(),
            Arc::new(Notify::new()),
            OutputEncoding::Auto,
        )
        .await;
        let buffer = output.lock().await;
        let (text, _, next) = buffer.bytes_since(0).unwrap();
        assert_eq!(text, "涓".as_bytes());
        assert_eq!(next, 3, "cursor counts normalized UTF-8 bytes");
    }

    #[test]
    fn validate_session_id_accepts_spawn_ids() {
        assert!(validate_session_id("sess_550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn validate_session_id_rejects_bad() {
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("nope").is_err());
        assert!(validate_session_id(&format!("sess_{}", "x".repeat(200))).is_err());
    }

    fn sample_task() -> CallbackTask {
        CallbackTask {
            session_id: "sess_abc".to_string(),
            command: "make test".to_string(),
            workdir: "/home/alice/project".to_string(),
            exit_code: 0,
            elapsed_secs: 42,
            output_encoding: OutputEncoding::Utf8,
        }
    }

    #[test]
    fn callback_payload_has_prd_shape() {
        let task = sample_task();
        let v = build_callback_payload(&task, "alice-laptop", b"hello", 5);

        assert_eq!(v["source"], "portal");
        assert_eq!(v["task_id"], "sess_abc");
        assert_eq!(v["summary"], "portal_exec completed: 'make test' (exit 0)");
        assert_eq!(v["result"]["session_id"], "sess_abc");
        assert_eq!(v["result"]["exit_code"], 0);
        assert_eq!(v["result"]["output"], "hello");
        assert_eq!(v["result"]["output_encoding"], "utf8");
        assert_eq!(v["result"]["command"], "make test");
        assert_eq!(v["result"]["workdir"], "/home/alice/project");
        assert_eq!(v["result"]["elapsed_secs"], 42);
        assert_eq!(v["result"]["portal_name"], "alice-laptop");
        assert_eq!(v["result"]["truncated"], false);
        assert_eq!(v["result"]["total_output_bytes"], 5);
    }

    #[test]
    fn callback_payload_takes_tail_and_marks_truncated() {
        let task = sample_task();
        let mut data = vec![b'a'; CALLBACK_OUTPUT_MAX_BYTES];
        data.extend_from_slice(b"THE_END");
        let total = data.len() as u64;

        let v = build_callback_payload(&task, "p", &data, total);
        let out = v["result"]["output"].as_str().unwrap();

        assert_eq!(out.len(), CALLBACK_OUTPUT_MAX_BYTES);
        assert!(out.ends_with("THE_END"), "tail must be kept, not the head");
        assert_eq!(v["result"]["truncated"], true);
        assert_eq!(v["result"]["total_output_bytes"], total);
    }

    #[test]
    fn callback_payload_truncated_when_ring_dropped_bytes() {
        let task = sample_task();
        // Ring buffer holds 5 bytes but 1000 were written overall.
        let v = build_callback_payload(&task, "p", b"tail!", 1000);
        assert_eq!(v["result"]["truncated"], true);
        assert_eq!(v["result"]["total_output_bytes"], 1000);
    }

    #[test]
    fn callback_payload_falls_back_when_escaping_blows_the_cap() {
        let task = sample_task();
        // Every byte escapes to 6 chars (\u00XX) — 200KB tail would be ~1.2MB.
        let data = vec![0x01u8; CALLBACK_OUTPUT_MAX_BYTES + 10];
        let v = build_callback_payload(&task, "p", &data, data.len() as u64);
        let out = v["result"]["output"].as_str().unwrap();
        assert_eq!(out.len(), CALLBACK_OUTPUT_FALLBACK_BYTES);
    }

    #[test]
    fn callback_payload_clamps_a_huge_command() {
        let mut task = sample_task();
        task.command = "é".repeat(10_000); // multi-byte: must not split a char
        let v = build_callback_payload(&task, "p", b"", 0);
        let cmd = v["result"]["command"].as_str().unwrap();
        assert!(cmd.len() < CALLBACK_COMMAND_MAX_BYTES + 32);
        assert!(cmd.ends_with("…[truncated]"));
        assert!(serde_json::to_vec(&v).unwrap().len() <= CALLBACK_PAYLOAD_MAX_BYTES);
    }

    #[test]
    fn retry_only_on_5xx() {
        assert!(should_retry_status(500));
        assert!(should_retry_status(503));
        assert!(!should_retry_status(200));
        assert!(!should_retry_status(401));
        assert!(!should_retry_status(413));
        assert!(!should_retry_status(404));
    }

    #[test]
    fn redact_url_strips_query() {
        assert_eq!(
            redact_url("https://echo.beings.town/alice/api/callback?token=secret"),
            "https://echo.beings.town/alice/api/callback?<redacted>"
        );
        assert_eq!(
            redact_url("https://echo.beings.town/alice/api/callback"),
            "https://echo.beings.town/alice/api/callback"
        );
    }

    // --- end-to-end delivery against a local HTTP server ---

    struct TestServer {
        url: String,
        hits: Arc<std::sync::Mutex<Vec<(serde_json::Value, Option<String>)>>>,
    }

    /// Spawn a one-route axum server that records callback bodies + auth headers.
    async fn start_test_server(status: axum::http::StatusCode) -> TestServer {
        use axum::extract::State;
        use axum::routing::post;

        type Hits = Arc<std::sync::Mutex<Vec<(serde_json::Value, Option<String>)>>>;
        let hits: Hits = Arc::new(std::sync::Mutex::new(Vec::new()));

        let app = axum::Router::new()
            .route(
                "/api/callback",
                post(
                    |State((hits, status)): State<(Hits, axum::http::StatusCode)>,
                     headers: axum::http::HeaderMap,
                     body: String| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        let v = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                        hits.lock().unwrap().push((v, auth));
                        status
                    },
                ),
            )
            .with_state((Arc::clone(&hits), status));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        TestServer {
            url: format!("http://{addr}/api/callback"),
            hits,
        }
    }

    /// Poll until the server has `n` hits, or give up after `timeout`.
    async fn wait_for_hits(server: &TestServer, n: usize, timeout: Duration) -> usize {
        let deadline = time::Instant::now() + timeout;
        loop {
            let got = server.hits.lock().unwrap().len();
            if got >= n || time::Instant::now() >= deadline {
                return got;
            }
            time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test]
    async fn callback_posted_on_process_exit() {
        use crate::config::PortalConfig;

        let server = start_test_server(axum::http::StatusCode::OK).await;
        let pm = ProcessManager::new();
        pm.set_callback_config(
            server.url.clone(),
            "tok_secret".to_string(),
            "alice-laptop".to_string(),
        );

        let config = PortalConfig::default();
        let info = pm
            .spawn(&config, "echo portal_callback_test", ".", &[])
            .await
            .unwrap();

        assert_eq!(wait_for_hits(&server, 1, Duration::from_secs(10)).await, 1);
        let (body, auth) = server.hits.lock().unwrap()[0].clone();

        assert_eq!(auth, None); // Heart callback auth is sent in the URL query.
        assert_eq!(body["source"], "portal");
        assert_eq!(body["task_id"], info.session_id);
        assert_eq!(body["result"]["exit_code"], 0);
        assert_eq!(body["result"]["portal_name"], "alice-laptop");
        assert_eq!(body["result"]["workdir"], ".");
        assert_eq!(body["result"]["truncated"], false);
        assert!(
            body["result"]["output"]
                .as_str()
                .unwrap()
                .contains("portal_callback_test")
        );
    }

    #[tokio::test]
    async fn no_callback_without_config() {
        use crate::config::PortalConfig;

        let server = start_test_server(axum::http::StatusCode::OK).await;
        let pm = ProcessManager::new();
        // Deliberately no set_callback_config — standalone mode.

        let config = PortalConfig::default();
        pm.spawn(&config, "echo standalone", ".", &[]).await.unwrap();

        assert_eq!(wait_for_hits(&server, 1, Duration::from_secs(2)).await, 0);
    }

    #[tokio::test]
    async fn killed_session_suppresses_callback() {
        use crate::config::PortalConfig;

        let server = start_test_server(axum::http::StatusCode::OK).await;
        let pm = ProcessManager::new();
        pm.set_callback_config(server.url.clone(), "tok".to_string(), "p".to_string());

        let config = PortalConfig::default();
        let info = pm.spawn(&config, "sleep 30", ".", &[]).await.unwrap();
        pm.kill(&info.session_id).await.unwrap();

        assert_eq!(wait_for_hits(&server, 1, Duration::from_secs(2)).await, 0);
    }

    #[tokio::test]
    async fn kill_all_suppresses_callback() {
        use crate::config::PortalConfig;

        let server = start_test_server(axum::http::StatusCode::OK).await;
        let pm = ProcessManager::new();
        pm.set_callback_config(server.url.clone(), "tok".to_string(), "p".to_string());

        let config = PortalConfig::default();
        pm.spawn(&config, "sleep 30", ".", &[]).await.unwrap();
        pm.kill_all().await;

        assert_eq!(wait_for_hits(&server, 1, Duration::from_secs(2)).await, 0);
    }

    #[tokio::test]
    async fn unauthorized_response_is_not_retried() {
        use crate::config::PortalConfig;

        let server = start_test_server(axum::http::StatusCode::UNAUTHORIZED).await;
        let pm = ProcessManager::new();
        pm.set_callback_config(server.url.clone(), "bad".to_string(), "p".to_string());

        let config = PortalConfig::default();
        pm.spawn(&config, "echo nope", ".", &[]).await.unwrap();

        assert_eq!(wait_for_hits(&server, 1, Duration::from_secs(10)).await, 1);
        // First retry would land 2s later — confirm it never comes.
        time::sleep(Duration::from_secs(3)).await;
        assert_eq!(server.hits.lock().unwrap().len(), 1);
    }
}
